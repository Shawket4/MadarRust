use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::kitchen::KitchenTicketView;
use crate::models::UserRole;
use crate::orders::handlers::Order;
use crate::realtime::hub::BranchEventHub;
use crate::tickets::OpenTicketView;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

async fn seed_table(pool: &PgPool, org: Uuid, branch: Uuid, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branch_tables (id, org_id, branch_id, label) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(org)
        .bind(branch)
        .bind(label)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn table_status(pool: &PgPool, table: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM v_table_status WHERE table_id = $1")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// A party sat down at `table` with no bill yet: a live `party` row in the
/// occupancy ledger, placed by `by`.
async fn seed_party_hold(pool: &PgPool, table: Uuid, by: Uuid) {
    sqlx::query(
        "INSERT INTO table_occupancies (org_id, branch_id, table_id, held_by, started_by) \
         SELECT org_id, branch_id, id, 'party', $2 FROM branch_tables WHERE id = $1",
    )
    .bind(table)
    .bind(by)
    .execute(pool)
    .await
    .unwrap();
}
/// The last party left plates on `table`: an ended row still owed a bus.
async fn seed_dirty(pool: &PgPool, table: Uuid) {
    sqlx::query(
        "INSERT INTO table_occupancies \
            (org_id, branch_id, table_id, held_by, started_at, ended_at, end_reason, needs_bussing) \
         SELECT org_id, branch_id, id, 'party', now() - interval '1 hour', \
                now() - interval '10 minutes', 'released', true \
           FROM branch_tables WHERE id = $1",
    )
    .bind(table)
    .execute(pool)
    .await
    .unwrap();
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
async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO shifts (branch_id, teller_id, status, opening_cash) VALUES ($1,$2,'open',0) RETURNING id")
        .bind(branch).bind(teller).fetch_one(pool).await.unwrap()
}
async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role).bind(resource).bind(action).execute(pool).await.unwrap();
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(crate::tickets::routes::configure)
                .configure(crate::kitchen::routes::configure)
                .configure(crate::sync::routes::configure),
        )
        .await
    };
}

/// A party sitting down is NOT a bill, and the first round is.
///
/// Seating used to open an empty ticket. That put a zero-value bill in every
/// report, and a party who changed their mind and left had to be VOIDED — as
/// though a sale had been undone. Occupancy travels on its own now
/// (`floor_ops::hold_table`), and the tab starts when somebody orders, taking
/// the table the party is already sitting at.
#[sqlx::test]
async fn the_first_round_claims_the_table_the_party_is_sitting_at(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 2500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T1").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    let t = token(teller, org, UserRole::Teller);

    // They sat down: the table is taken, and there is no bill anywhere.
    seed_party_hold(&pool, table, teller).await;
    assert_eq!(table_status(&pool, table).await, "seated");
    let tickets: i64 = sqlx::query_scalar("SELECT count(*) FROM open_tickets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tickets, 0, "sitting down is not a bill");

    // Their first round starts the tab ON that table — `seated` with no ticket
    // on it is claimable, and this is who it was being held for.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "table_id": table,
                "guest_count": 2,
                "items": [{ "menu_item_id": item, "quantity": 2 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(view.table_id, Some(table), "the tab took their table");
    assert_eq!(view.subtotal, 5000, "two of them, one round");
    assert_eq!(table_status(&pool, table).await, "seated");
    // The hold became the ticket: its row ended `seated`, the ticket's is live.
    let rows: Vec<(String, Option<String>, Option<Uuid>)> = sqlx::query_as(
        "SELECT held_by, end_reason, open_ticket_id FROM table_occupancies \
          WHERE table_id = $1 ORDER BY started_at, id",
    )
    .bind(table)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("party".into(), Some("seated".into()), None),
            ("ticket".into(), None, Some(view.id)),
        ]
    );
}

/// A tab with nothing on it is not a thing. It was, briefly, and it is what
/// seating used to create.
#[sqlx::test]
async fn a_ticket_must_carry_at_least_one_item(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T1").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    let t = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch, "table_id": table, "items": []
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 400);
    assert_eq!(
        table_status(&pool, table).await,
        "free",
        "and nothing moved"
    );
}

/// An UNBUSSED table is not claimable by a round. A fire absorbs a table it
/// cannot have rather than dead-lettering, so the ticket simply floats
/// table-less and the waiter reassigns it — but it must never silently clear
/// somebody else's plates by seating a new party on them.
#[sqlx::test]
async fn a_round_will_not_claim_a_table_nobody_has_bussed(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T1").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    let t = token(teller, org, UserRole::Teller);

    seed_dirty(&pool, table).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "table_id": table,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "the round still lands");
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(view.table_id, None, "but not on the dirty table");
    assert_eq!(
        table_status(&pool, table).await,
        "dirty",
        "still needs a cloth"
    );
}

/// Full chain: a waiter fires a dine-in ticket → it lands on the KDS → a cook
/// bumps it → the ticket goes ready → a cashier settles it into a paid dine-in
/// order in THEIR shift. Then a double-settle is a clean conflict.
#[sqlx::test]
async fn waiter_fire_bump_settle_end_to_end(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;

    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "waiter", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    grant(&pool, "teller", "orders", "create").await;
    grant(&pool, "teller", "payments", "create").await;
    grant(&pool, "teller", "kitchen_orders", "read").await;
    grant(&pool, "teller", "kitchen_orders", "update").await;

    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // 1. Waiter fires a ticket (2× a 1000-piastre item).
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "items": [{ "menu_item_id": item, "quantity": 2 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "waiter fires a ticket");
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(view.status, "open");
    assert_eq!(view.items.len(), 1);
    assert_eq!(view.subtotal, 2000);
    let ticket_id = view.id;
    assert_eq!(
        view.opened_by, waiter,
        "waiter is preserved as the order-taker"
    );

    // A kitchen ticket was emitted for this open ticket.
    let kt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kitchen_tickets WHERE source_type='open_ticket' AND source_id=$1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kt, 1);

    // 2. The line shows on the KDS feed.
    let feed_resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/kitchen/orders?branch_id={branch}"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    assert_eq!(feed_resp.status(), 200);
    let feed: Vec<KitchenTicketView> = test::read_body_json(feed_resp).await;
    assert_eq!(feed.len(), 1, "one outstanding kitchen ticket");
    let kitchen_item_id = feed[0].items[0].id;

    // 3. Bump it → the kitchen is done. The BILL stays `open` (readiness is not
    //    a bill state); the kitchen ticket closes `bumped` and the view derives
    //    `ready` from it.
    let bump = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/kitchen/items/{kitchen_item_id}/bump"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    assert_eq!(bump.status(), 204);
    let status: String = sqlx::query_scalar("SELECT status::text FROM open_tickets WHERE id=$1")
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        status, "open",
        "the bill is still unpaid, whatever the kitchen did"
    );
    let (kt_status, close_reason, closed_by): (String, Option<String>, Option<Uuid>) =
        sqlx::query_as(
            "SELECT status::text, close_reason::text, closed_by \
             FROM kitchen_tickets WHERE open_ticket_id = $1",
        )
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kt_status, "ready");
    assert_eq!(
        close_reason.as_deref(),
        Some("bumped"),
        "the last bump closes the ticket"
    );
    assert_eq!(closed_by, Some(teller), "by whoever bumped it");
    let get = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/open-tickets/{ticket_id}"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(get).await;
    assert!(view.ready, "readiness is derived from the kitchen tickets");
    assert!(
        view.ready_at.is_some(),
        "and the moment is kept on the bill as history"
    );

    // 4. Cashier settles into THEIR shift → a paid dine-in order.
    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({ "shift_id": shift, "payment_method": "cash" }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 200, "cashier settles");
    let order: Order = test::read_body_json(settle).await;
    assert_eq!(order.order_type, "dine_in");
    assert_eq!(order.subtotal, 2000, "2 × 1000 items");
    assert!(order.total_amount >= 2000, "total includes any org tax");
    assert_eq!(
        order.shift_id, shift,
        "lands in the settling cashier's shift"
    );
    assert_eq!(order.teller_id, teller);
    // The paid order is stamped with the WAITER who opened the ticket (not the
    // settling cashier) — this is what the dashboard segments/exports by. Direct
    // POS sales leave it null; here it must resolve to the ticket's opener.
    assert_eq!(
        order.waiter_id,
        Some(waiter),
        "settled order carries the ticket's waiter (opened_by)"
    );
    assert!(
        order.waiter_name.is_some(),
        "waiter_name is joined from users for the dashboard"
    );

    let (st, oid): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status::text, order_id FROM open_tickets WHERE id=$1")
            .bind(ticket_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(st, "settled");
    assert_eq!(oid, Some(order.id));
    // Both sides of the link, written in the order's own transaction.
    let back: Option<Uuid> = sqlx::query_scalar("SELECT open_ticket_id FROM orders WHERE id = $1")
        .bind(order.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        back,
        Some(ticket_id),
        "the order points back at the bill it settled"
    );
    // The kitchen ticket was already closed `bumped`; the settle does not
    // rewrite that — the first close wins.
    let reason: Option<String> = sqlx::query_scalar(
        "SELECT close_reason::text FROM kitchen_tickets WHERE open_ticket_id = $1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(reason.as_deref(), Some("bumped"));

    // 5. Double settle is a clean conflict.
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({ "shift_id": shift, "payment_method": "cash" }))
            .to_request(),
    )
    .await;
    assert_eq!(again.status(), 409, "already settled");
}

/// Firing requires the branch to be operating (a till open) — no shift → 409.
#[sqlx::test]
async fn fire_requires_open_shift_at_branch(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);

    let resp = test::call_service(&app, test::TestRequest::post()
        .uri("/open-tickets")
        .insert_header(("Authorization", format!("Bearer {waiter_t}")))
        .set_json(&serde_json::json!({ "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }))
        .to_request()).await;
    assert_eq!(resp.status(), 409, "no open shift → cannot fire");
}

/// Offline replay (WS2g): a queued waiter fire → round → cashier settle flushes
/// through `/sync/replay`, attributed to the embedded actor (not the bearer), and
/// each op is idempotent on its client-minted key — a lost-ack retry produces no
/// duplicate ticket / round / order.
#[sqlx::test]
async fn replay_fire_round_settle_idempotent_and_attributed(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    // The device flushing the backlog signs in as a teller; replay authorizes via
    // each op's EMBEDDED actor — and now ENFORCES that actor's permissions (audit
    // #12), exactly like the live routes — so grant each embedded actor the role
    // default its replayed op requires.
    grant(&pool, "waiter", "open_tickets", "create").await; // fire
    grant(&pool, "waiter", "open_tickets", "update").await; // add round
    grant(&pool, "teller", "open_tickets", "update").await; // settle
    grant(&pool, "teller", "orders", "create").await; // settle materializes the order
    grant(&pool, "teller", "payments", "create").await; // ...and takes the money
    let bearer = token(teller, org, UserRole::Teller);

    let ticket_idem = Uuid::new_v4();
    let round1_idem = Uuid::new_v4();
    let fire = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": waiter,
        "request": {
            "branch_id": branch,
            "idempotency_key": ticket_idem,
            "round_idempotency_key": round1_idem,
            "items": [{ "menu_item_id": item, "quantity": 2 }]
        }
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&fire)
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201, "replayed fire creates the ticket");
    let view: OpenTicketView = test::read_body_json(resp).await;
    let ticket_id = view.id;
    assert_eq!(
        view.opened_by, waiter,
        "attributed to the embedded waiter, not the bearer"
    );
    assert_eq!(view.subtotal, 2000);

    // Lost-ack retry of the SAME fire → dedups (no second ticket).
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&fire)
            .to_request(),
    )
    .await;
    assert!(again.status().is_success(), "replayed fire is idempotent");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM open_tickets WHERE idempotency_key=$1")
        .bind(ticket_idem)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "exactly one ticket for the idempotency key");

    // Replay a second round (its own key), twice → dedups.
    let round2_idem = Uuid::new_v4();
    let round = serde_json::json!({
        "op": "add_ticket_round",
        "teller_id": waiter,
        "ticket_id": ticket_id,
        "request": { "idempotency_key": round2_idem, "items": [{ "menu_item_id": item, "quantity": 1 }] }
    });
    for _ in 0..2 {
        let r = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/sync/replay")
                .insert_header(("Authorization", format!("Bearer {bearer}")))
                .set_json(&round)
                .to_request(),
        )
        .await;
        assert!(r.status().is_success(), "replayed round ok");
    }
    let rounds: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM open_ticket_rounds WHERE open_ticket_id=$1")
            .bind(ticket_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rounds, 2, "round 1 (fire) + round 2, no duplicate");

    // Replay the cashier settle, twice → one paid order in the cashier's shift.
    let settle = serde_json::json!({
        "op": "settle_open_ticket",
        "teller_id": teller,
        "ticket_id": ticket_id,
        "request": { "shift_id": shift, "payment_method": "cash" }
    });
    let s1 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&settle)
            .to_request(),
    )
    .await;
    assert_eq!(s1.status(), 200, "replayed settle ok");
    let order: Order = test::read_body_json(s1).await;
    assert_eq!(order.shift_id, shift, "lands in the cashier's shift");
    assert_eq!(order.teller_id, teller);
    assert_eq!(order.subtotal, 3000, "2×1000 + 1×1000 across both rounds");

    let s2 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&settle)
            .to_request(),
    )
    .await;
    assert_eq!(
        s2.status(),
        200,
        "replayed settle is idempotent (lost ack), not a 409"
    );
    let order2: Order = test::read_body_json(s2).await;
    assert_eq!(order2.id, order.id, "same paid order returned");
    let orders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM orders WHERE idempotency_key=$1")
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orders, 1, "exactly one order materialized");
}

/// A TELLER ringing up a table is the ordinary act the floor is for, and it has
/// to survive the queue.
///
/// The tables screen is where a teller takes a party's round, and under the
/// "every order must have a table" toggle that screen is the teller's HOME. The
/// replay gate said firing was waiter-only — right while only the waiter app
/// fired — so every round a teller queued came back "Replay actor may not
/// perform this operation for this organization".
///
/// It was also stricter than the live route, which is the one thing this gate
/// must never be: `create_open_ticket` checks `open_tickets:create` and no role
/// at all. Since the POS drains every write through `/sync/replay`, stricter
/// than live means impossible.
#[sqlx::test]
async fn a_teller_can_ring_up_a_table_through_the_queue(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T1").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    let bearer = token(teller, org, UserRole::Teller);

    // The party is already sitting there; this is their first round arriving.
    seed_party_hold(&pool, table, teller).await;
    let seat = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "table_id": table,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&seat)
            .to_request(),
    )
    .await;
    assert!(
        r.status().is_success(),
        "a teller rings up a table, got {}",
        r.status()
    );

    let status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "seated", "the room knows the table is taken");
}

/// And the permission table is still the authority. Revoke a teller's
/// `open_tickets:create` and the queue is not a way around it — which is the
/// whole reason `required_permissions` re-checks what the live endpoint checks.
#[sqlx::test]
async fn a_teller_denied_open_tickets_cannot_ring_up_through_the_queue_either(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let table = seed_table(&pool, org, branch, "T1").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    // A per-user deny beats the role grant, live or replayed.
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) \
         VALUES ($1, 'open_tickets'::permission_resource, 'create'::permission_action, false)",
    )
    .bind(teller)
    .execute(&pool)
    .await
    .unwrap();
    let bearer = token(teller, org, UserRole::Teller);

    let seat = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "table_id": table,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&seat)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "a revoked grant still refuses");

    let status: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "free", "and the table was not taken");
}

/// Replay attribution-safety: an op can only be replayed under a role that could
/// have produced it live, and only for an actor in the bearer's org.
#[sqlx::test]
async fn replay_rejects_wrong_actor_role_or_org(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let bearer = token(teller, org, UserRole::Teller);

    // A KITCHEN device cannot be the actor of a fire. It bumps what arrives;
    // it never opens a tab, so an op attributed to one could not have happened.
    let kitchen = seed_user(&pool, org, "kitchen").await;
    let fire_by_kitchen = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": kitchen,
        "request": { "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&fire_by_kitchen)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "a kitchen device may not open a tab");

    // A WAITER cannot be the actor of a settle (settling is teller-only).
    let settle_by_waiter = serde_json::json!({
        "op": "settle_open_ticket",
        "teller_id": waiter,
        "ticket_id": Uuid::new_v4(),
        "request": { "shift_id": Uuid::new_v4(), "payment_method": "cash" }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&settle_by_waiter)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "waiter may not settle");

    // An actor from a DIFFERENT org is rejected before any dispatch.
    let other_org = seed_org(&pool).await;
    let other_waiter = seed_user(&pool, other_org, "waiter").await;
    let cross = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": other_waiter,
        "request": { "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }
    });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&cross)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 403, "actor from another org rejected");
}

/// Phase E offline display: a fire derives its kitchen-ticket + line ids from the
/// round's CLIENT idempotency key, so a device that fired offline predicted the SAME
/// ids (its KDS projection + a later bump reconcile against the server row on sync).
#[sqlx::test]
async fn fire_derives_stable_kitchen_ids_from_round_key(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);

    let round_idem = Uuid::new_v4();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "idempotency_key": Uuid::new_v4(),
                "round_idempotency_key": round_idem,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(resp).await.id;

    let kt = crate::kitchen::derive_kitchen_ticket_id(round_idem);
    let (got_kt, got_item): (Uuid, Uuid) = sqlx::query_as(
        "SELECT kt.id, kti.id FROM kitchen_tickets kt \
         JOIN kitchen_ticket_items kti ON kti.kitchen_ticket_id = kt.id \
         WHERE kt.source_type='open_ticket' AND kt.source_id=$1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(got_kt, kt, "kitchen ticket id derived from the round key");
    assert_eq!(
        got_item,
        crate::kitchen::derive_kitchen_item_id(kt, 0),
        "line 0 id derived"
    );
}

/// Offline bump replay (Phase E step 2): a KDS bump queued while offline flushes
/// through `/sync/replay`, attributed to the embedded KITCHEN actor (not the
/// bearer), idempotent on the `item_id`. A bump for a gone/unknown line replays as
/// a clean 204 no-op so it can never wedge the FIFO drain. A waiter may not bump.
#[sqlx::test]
async fn replay_bump_idempotent_and_attributed(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let kitchen = seed_user(&pool, org, "kitchen").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    // Replay now enforces the embedded actor's permissions (audit #12). The bump
    // lands under the kitchen user, so grant it kitchen_orders/update.
    grant(&pool, "kitchen", "kitchen_orders", "update").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let bearer = token(teller, org, UserRole::Teller);

    // Waiter fires a ticket live → a kitchen ticket + one line.
    let resp = test::call_service(&app, test::TestRequest::post()
        .uri("/open-tickets")
        .insert_header(("Authorization", format!("Bearer {waiter_t}")))
        .set_json(&serde_json::json!({ "branch_id": branch, "items": [{ "menu_item_id": item, "quantity": 1 }] }))
        .to_request()).await;
    assert_eq!(resp.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(resp).await.id;

    // The (server-minted) kitchen line id the KDS would have bumped.
    let kitchen_item_id: Uuid = sqlx::query_scalar(
        "SELECT kti.id FROM kitchen_ticket_items kti \
         JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
         WHERE kt.source_type='open_ticket' AND kt.source_id=$1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    // Replay the bump attributed to the KITCHEN device.
    let bump = serde_json::json!({ "op": "bump_kitchen_item", "teller_id": kitchen, "item_id": kitchen_item_id });
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&bump)
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 204, "replayed bump ok");
    // Readiness lives on the KITCHEN ticket (and closes it, `bumped`); the bill
    // itself stays `open`.
    let (status, kt_status, close_reason, bumped_by): (
        String,
        String,
        Option<String>,
        Option<Uuid>,
    ) = sqlx::query_as(
        "SELECT ot.status::text, kt.status::text, kt.close_reason::text, kti.bumped_by \
             FROM open_tickets ot, kitchen_tickets kt, kitchen_ticket_items kti \
             WHERE ot.id=$1 AND kt.open_ticket_id = ot.id AND kti.id=$2",
    )
    .bind(ticket_id)
    .bind(kitchen_item_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "open", "the bill is not a kitchen state");
    assert_eq!(
        kt_status, "ready",
        "only line bumped → kitchen ticket ready"
    );
    assert_eq!(close_reason.as_deref(), Some("bumped"));
    assert_eq!(
        bumped_by,
        Some(kitchen),
        "attributed to the embedded kitchen actor, not the bearer"
    );

    // Idempotent: re-bump the same line → 204 no-op (still ready, still kitchen).
    let r2 = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&bump)
            .to_request(),
    )
    .await;
    assert_eq!(r2.status(), 204, "re-bump is an idempotent no-op");

    // A bump for an UNKNOWN line replays as a clean no-op (never wedges the drain).
    let ghost = serde_json::json!({ "op": "bump_kitchen_item", "teller_id": kitchen, "item_id": Uuid::new_v4() });
    let rg = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&ghost)
            .to_request(),
    )
    .await;
    assert_eq!(
        rg.status(),
        204,
        "bump for a gone line is a no-op, not an error"
    );

    // A WAITER may not be the actor of a bump (bump is kitchen/teller only).
    let bump_by_waiter = serde_json::json!({ "op": "bump_kitchen_item", "teller_id": waiter, "item_id": kitchen_item_id });
    let rw = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&bump_by_waiter)
            .to_request(),
    )
    .await;
    assert_eq!(rw.status(), 403, "waiter may not bump");

    // Unbump replay → the line reopens, the ticket falls back to open.
    let unbump = serde_json::json!({ "op": "unbump_kitchen_item", "teller_id": kitchen, "item_id": kitchen_item_id });
    let ru = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&unbump)
            .to_request(),
    )
    .await;
    assert_eq!(ru.status(), 204, "replayed unbump ok");
    let (kt_status, closed_at): (String, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status::text, closed_at FROM kitchen_tickets WHERE open_ticket_id = $1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        kt_status, "firing",
        "unbumped line → kitchen ticket back to firing"
    );
    assert!(closed_at.is_none(), "a recall reopens a `bumped` close");
}

/// Regression — the "nothing happens on the teller side" bug. A waiter device
/// fires offline-first, so the fire reaches the backend via `/sync/replay` (NOT
/// the direct `POST /open-tickets`), even when the waiter is online. The replay
/// path must STILL publish to the branch bus, or a connected teller/KDS gets no
/// live push (and no ping/notification) until it manually reloads. We subscribe
/// to the branch bus AS a connected teller would and assert the fire emits
/// `ticket.fired` (+ `kitchen.fired` for the KDS).
#[sqlx::test]
async fn replay_fire_publishes_realtime(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "waiter", "open_tickets", "read").await;

    // Hold a hub handle to subscribe as a connected teller; the app shares the
    // SAME hub (Arc inside), so a publish from the replay handler reaches `rx`.
    let hub = BranchEventHub::new();
    let mut rx = hub.subscribe(branch);
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(hub.clone()))
            .configure(crate::tickets::routes::configure)
            .configure(crate::kitchen::routes::configure)
            .configure(crate::sync::routes::configure),
    )
    .await;

    let waiter_t = token(waiter, org, UserRole::Waiter);
    let fire = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": waiter,
        "request": {
            "branch_id": branch,
            "items": [{ "menu_item_id": item, "quantity": 1 }],
            "idempotency_key": Uuid::new_v4(),
        }
    });
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&fire)
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "replayed fire ok: {}",
        resp.status()
    );

    // Drain what the branch bus received. Before the fix this was empty.
    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        kinds.push(ev.event_type);
    }
    assert!(
        kinds.iter().any(|k| k == "ticket.fired"),
        "replayed fire must publish ticket.fired (got {kinds:?})"
    );
    assert!(
        kinds.iter().any(|k| k == "kitchen.fired"),
        "replayed fire must publish kitchen.fired for the KDS (got {kinds:?})"
    );
}

/// A reward on a ticket names its LINE, and the server turns that into the
/// position the line lands at.
///
/// The ordering (`round_number, created_at`) is decided in the settle handler,
/// so only the server can know it. A till that guessed an index would take the
/// wrong item off the bill — silently, and in the customer's favour or not.
/// This pins the translation with a ticket whose SECOND round holds the reward,
/// which is exactly where a naive "index = position in the round" would break.
#[sqlx::test]
async fn a_reward_on_a_ticket_covers_the_line_it_names(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let cheap = seed_menu_item(&pool, org, 1_000).await;
    let latte = seed_menu_item(&pool, org, 5_000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;

    for (role, res, act) in [
        ("waiter", "open_tickets", "create"),
        ("waiter", "open_tickets", "update"),
        ("teller", "open_tickets", "update"),
        ("teller", "orders", "create"),
        ("teller", "payments", "create"),
        ("teller", "loyalty", "update"),
    ] {
        grant(&pool, role, res, act).await;
    }
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // A stamp card, and the latte is what a stamp buys.
    sqlx::query(
        "INSERT INTO loyalty_settings (org_id, branch_id, enabled, mode, default_reward_cost) \
         VALUES ($1, NULL, true, 'visits', 5)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO loyalty_reward_items (org_id, menu_item_id, cost_currency, cost_amount) \
         VALUES ($1,$2,'visits',5)",
    )
    .bind(org)
    .bind(latte)
    .execute(&pool)
    .await
    .unwrap();
    let member: Uuid = sqlx::query_scalar(
        "INSERT INTO loyalty_customers (org_id, phone, name, member_token) \
         VALUES ($1,'201000000001','Ali','Mticketline000000001') RETURNING id",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points) \
         VALUES ($1,$2,$3,'adjust','visits',5)",
    )
    .bind(org)
    .bind(member)
    .bind(branch)
    .execute(&pool)
    .await
    .unwrap();

    // Round 1: the cheap item. Round 2: the latte.
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "items": [{ "menu_item_id": cheap, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let view: OpenTicketView = test::read_body_json(resp).await;
    let ticket_id = view.id;

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/rounds"))
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "items": [{ "menu_item_id": latte, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert!(
        resp.status().is_success(),
        "second round: {}",
        resp.status()
    );

    // The latte's LINE id — what the till would send.
    let latte_line: Uuid = sqlx::query_scalar(
        "SELECT oti.id FROM open_ticket_items oti \
           JOIN open_ticket_rounds r ON r.id = oti.round_id \
          WHERE oti.open_ticket_id = $1 AND oti.menu_item_id = $2",
    )
    .bind(ticket_id)
    .bind(latte)
    .fetch_one(&pool)
    .await
    .unwrap();

    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket_id}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift,
                "payment_method": "cash",
                "loyalty_customer_id": member,
                "loyalty_redemptions": [{ "ticket_line_id": latte_line }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 200, "cashier settles with a reward");
    let order: Order = test::read_body_json(settle).await;

    // The LATTE went free, not the cheap item that sits first in the list.
    assert_eq!(order.subtotal, 1_000, "only the cheap line is charged");
    let free: Vec<(String, i32, bool)> = sqlx::query_as(
        "SELECT item_name, line_total, is_reward FROM order_items \
          WHERE order_id = $1 ORDER BY line_total",
    )
    .bind(order.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        free.iter().any(|(_, total, reward)| *total == 0 && *reward),
        "the covered line is free and marked: {free:?}"
    );
    let balance: i32 =
        sqlx::query_scalar("SELECT visits_balance FROM loyalty_customers WHERE id = $1")
            .bind(member)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balance, 0, "five stamps spent");
}

/// A positional index on a ticket settle is refused outright.
///
/// The till cannot know the server's ordering, so an index it supplied would be
/// a guess. Rejecting is the only safe answer — the alternative is giving away
/// whichever item happened to land there.
#[sqlx::test]
async fn a_ticket_reward_without_a_line_id_is_refused(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1_000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    for (role, res, act) in [
        ("waiter", "open_tickets", "create"),
        ("teller", "open_tickets", "update"),
        ("teller", "orders", "create"),
        ("teller", "payments", "create"),
        ("teller", "loyalty", "update"),
    ] {
        grant(&pool, role, res, act).await;
    }
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(resp).await;

    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/settle", view.id))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift,
                "payment_method": "cash",
                "loyalty_redemptions": [{ "item_index": 0 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 400, "a guessed index is refused");
}

// ── Settle and void as real transactions ─────────────────────────────────────

/// A confirmed booking for a party of two, arriving about now.
async fn seed_booking(pool: &PgPool, org: Uuid, branch: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, guest_phone) \
         VALUES ($1, $2, 2, now(), now() + interval '2 hours', 'Ali', '2010') RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn seed_card_method(pool: &PgPool, org: Uuid) {
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'card', '#000', 'card', false, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
}
async fn booking_state(pool: &PgPool, id: Uuid) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status::text, cancelled_by, cancel_reason FROM bookings WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn kitchen_state(
    pool: &PgPool,
    ticket: Uuid,
) -> (String, Option<String>, Option<Uuid>, bool) {
    sqlx::query_as(
        "SELECT status::text, close_reason::text, closed_by, closed_at IS NOT NULL \
         FROM kitchen_tickets WHERE open_ticket_id = $1",
    )
    .bind(ticket)
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn occupancy_end(pool: &PgPool, ticket: Uuid) -> (Option<String>, bool, Option<Uuid>, bool) {
    sqlx::query_as(
        "SELECT end_reason, needs_bussing, ended_by, ended_at IS NOT NULL \
         FROM table_occupancies WHERE open_ticket_id = $1",
    )
    .bind(ticket)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A void is ONE event with every consequence inside it.
///
/// Before this, tearing a bill up flipped its status and left its kitchen
/// copies `firing`, its table `seated` and its booking `seated` — three
/// screens each telling a different story about a party that had left. Every
/// table a ticket void touches is asserted here; the test is the deliverable
/// as much as the fix.
#[sqlx::test]
async fn voiding_a_ticket_tears_down_everything_it_held(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let table = seed_table(&pool, org, branch, "T1").await;
    let booking = seed_booking(&pool, org, branch).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    grant(&pool, "teller", "open_tickets", "delete").await; // void
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // The booked party sits at T1 and orders.
    let fire = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch, "table_id": table, "booking_id": booking,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(fire.status(), 201);
    let view: OpenTicketView = test::read_body_json(fire).await;
    let ticket = view.id;
    assert_eq!(
        view.table_id,
        Some(table),
        "the party's table was claimable"
    );

    // What the void has to undo.
    assert_eq!(booking_state(&pool, booking).await.0, "seated");
    assert_eq!(table_status(&pool, table).await, "seated");
    let (k_status, k_reason, _, k_closed) = kitchen_state(&pool, ticket).await;
    assert_eq!(
        (k_status.as_str(), k_reason, k_closed),
        ("firing", None, false)
    );

    let void = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/void"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(
                &serde_json::json!({ "reason": "wrong_order", "note": "rang the wrong table" }),
            )
            .to_request(),
    )
    .await;
    assert_eq!(void.status(), 200, "the teller voids the bill");

    // The bill: an event with an actor, a reason and a note.
    let (status, voided_by, reason, note, voided): (
        String,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        bool,
    ) = sqlx::query_as(
        "SELECT status::text, voided_by, void_reason::text, void_note, voided_at IS NOT NULL \
             FROM open_tickets WHERE id = $1",
    )
    .bind(ticket)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "voided");
    assert_eq!(voided_by, Some(teller), "the void names who tore it up");
    assert_eq!(reason.as_deref(), Some("wrong_order"));
    assert_eq!(note.as_deref(), Some("rang the wrong table"));
    assert!(voided);

    // The kitchen: the round is voided, closed `voided`, by the voider, and
    // every line is off the queue.
    let (k_status, k_reason, k_by, k_closed) = kitchen_state(&pool, ticket).await;
    assert_eq!(k_status, "voided");
    assert_eq!(k_reason.as_deref(), Some("voided"));
    assert_eq!(k_by, Some(teller));
    assert!(k_closed);
    let live_lines: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kitchen_ticket_items i JOIN kitchen_tickets kt ON kt.id = i.kitchen_ticket_id \
         WHERE kt.open_ticket_id = $1 AND i.voided_at IS NULL",
    )
    .bind(ticket)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        live_lines, 0,
        "no line of a voided round is left for a station to cook"
    );

    // The floor: the occupancy ended `voided`, nothing to bus, table free.
    let (end_reason, needs_bussing, ended_by, ended) = occupancy_end(&pool, ticket).await;
    assert_eq!(end_reason.as_deref(), Some("voided"));
    assert!(!needs_bussing, "nobody ate, nothing to bus");
    assert_eq!(ended_by, Some(teller));
    assert!(ended);
    assert_eq!(table_status(&pool, table).await, "free");

    // The booking: the party did not eat under it — cancelled, by the system.
    let (b_status, b_by, b_reason) = booking_state(&pool, booking).await;
    assert_eq!(b_status, "cancelled");
    assert_eq!(b_by.as_deref(), Some("system"));
    assert_eq!(b_reason.as_deref(), Some("Ticket voided"));

    // Idempotent: a lost-ack retry returns the voided bill and re-stamps nothing.
    let again = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/void"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({ "reason": "other", "note": "retry" }))
            .to_request(),
    )
    .await;
    assert_eq!(again.status(), 200);
    let note_after: Option<String> =
        sqlx::query_scalar("SELECT void_note FROM open_tickets WHERE id = $1")
            .bind(ticket)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(note_after.as_deref(), Some("rang the wrong table"));
}

/// The bill the till shows is the bill the books record.
///
/// The till used to show (and collect) the ticket's running subtotal while the
/// settle booked subtotal − discount + service charge + tax, so every table
/// sale left the drawer short by exactly the tax. Now the view carries the
/// server-priced bill, the settle refuses the old figure through the same
/// drift check a counter checkout gets, and the tenders, the change and the
/// time the till says it was paid all land on the order — with the floor
/// ended in the same commit.
#[sqlx::test]
async fn the_bill_the_till_sees_is_the_bill_the_books_record(pool: PgPool) {
    use chrono::Timelike;
    let app = app!(pool);
    let org = seed_org(&pool).await;
    // 14% exclusive (the org default) plus a 10% service charge, taxed.
    sqlx::query("UPDATE organizations SET service_charge_rate = 0.10, service_charge_taxable = true WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    seed_card_method(&pool, org).await;
    let table = seed_table(&pool, org, branch, "T1").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    grant(&pool, "teller", "orders", "create").await;
    grant(&pool, "teller", "payments", "create").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // 2 × 1000 with the waiter's 10% off.
    let fire = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {waiter_t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch, "table_id": table,
                "discount_type": "percentage", "discount_value": "0.10",
                "items": [{ "menu_item_id": item, "quantity": 2 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(fire.status(), 201);
    let view: OpenTicketView = test::read_body_json(fire).await;
    let ticket = view.id;

    // The bill, server-priced: 2000 − 200 = 1800; +10% = 180; 14% of 1980 = 277.
    let get = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/open-tickets/{ticket}"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(get).await;
    assert_eq!(view.subtotal, 2000);
    assert_eq!(view.bill.subtotal, 2000);
    assert_eq!(view.bill.discount_amount, 200);
    assert_eq!(view.bill.service_charge_amount, 180);
    assert_eq!(view.bill.tax_amount, 277);
    assert!(!view.bill.tax_inclusive);
    assert_eq!(view.bill.total, 2257, "what the drawer must collect");

    // The old figure — the subtotal — is refused, not booked.
    let short = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift, "payment_method": "cash", "total_amount": 2000
            }))
            .to_request(),
    )
    .await;
    assert_eq!(
        short.status(),
        409,
        "a till that collected the subtotal is out of step with the books"
    );
    let still_open: String =
        sqlx::query_scalar("SELECT status::text FROM open_tickets WHERE id = $1")
            .bind(ticket)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_open, "open");

    // Paid ten minutes ago as the till says, half by card, 43 back in change.
    let settled_at = (chrono::Utc::now() - chrono::Duration::minutes(10))
        .with_nanosecond(0)
        .unwrap();
    let settle = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift, "payment_method": "cash",
                "total_amount": view.bill.total,
                "payment_splits": [
                    { "method": "cash", "amount": 1257 },
                    { "method": "card", "amount": 1000 }
                ],
                "amount_tendered": 1300, "change_given": 43,
                "settled_at": settled_at
            }))
            .to_request(),
    )
    .await;
    assert_eq!(settle.status(), 200);
    let order: Order = test::read_body_json(settle).await;
    assert_eq!(
        order.total_amount, view.bill.total,
        "the books record what the till showed"
    );
    assert_eq!(order.discount_amount, 200);
    assert_eq!(order.service_charge_amount, 180);
    assert_eq!(order.tax_amount, 277);
    assert_eq!(order.amount_tendered, Some(1300));
    assert_eq!(order.change_given, Some(43));
    assert_eq!(
        order.created_at, settled_at,
        "the sale is dated when the till says it was paid"
    );

    // The split tenders survived — two legs, and the badge says so.
    let legs: Vec<(String, i32)> = sqlx::query_as(
        "SELECT method, amount FROM order_payments WHERE order_id = $1 ORDER BY amount DESC",
    )
    .bind(order.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(legs, vec![("cash".into(), 1257), ("card".into(), 1000)]);
    assert_eq!(order.payment_method, "mixed");

    // One instant on both rows.
    let ticket_settled_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT settled_at FROM open_tickets WHERE id = $1")
            .bind(ticket)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ticket_settled_at, Some(settled_at));

    // The floor, in the same commit: the party checked out, the table is
    // theirs to bus, the kitchen copy closed with the sale.
    let (end_reason, needs_bussing, ended_by, _) = occupancy_end(&pool, ticket).await;
    assert_eq!(end_reason.as_deref(), Some("settled"));
    assert!(needs_bussing);
    assert_eq!(ended_by, Some(teller));
    assert_eq!(table_status(&pool, table).await, "dirty");
    let (_, k_reason, k_by, _) = kitchen_state(&pool, ticket).await;
    assert_eq!(k_reason.as_deref(), Some("settled"));
    assert_eq!(k_by, Some(teller));

    // After the settle the view's bill is what was BOOKED, not a repricing.
    let get = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/open-tickets/{ticket}"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .to_request(),
    )
    .await;
    let after: OpenTicketView = test::read_body_json(get).await;
    assert_eq!(after.bill.total, order.total_amount);
}

/// The settle-time discount is TYPED. Silence inherits the waiter's; the
/// literal `"none"` clears it; anything else replaces it. It used to be
/// `.or()`, so a cashier could neither see nor clear what the waiter applied.
#[sqlx::test]
async fn a_cashier_inherits_clears_or_replaces_the_waiters_discount(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    grant(&pool, "teller", "orders", "create").await;
    grant(&pool, "teller", "payments", "create").await;
    let waiter_t = token(waiter, org, UserRole::Waiter);
    let teller_t = token(teller, org, UserRole::Teller);

    // Three identical bills, each with the waiter's 10% on it.
    let mut tickets = Vec::new();
    for _ in 0..3 {
        let fire = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/open-tickets")
                .insert_header(("Authorization", format!("Bearer {waiter_t}")))
                .set_json(&serde_json::json!({
                    "branch_id": branch,
                    "discount_type": "percentage", "discount_value": "0.10",
                    "items": [{ "menu_item_id": item, "quantity": 2 }]
                }))
                .to_request(),
        )
        .await;
        assert_eq!(fire.status(), 201);
        let v: OpenTicketView = test::read_body_json(fire).await;
        assert_eq!(
            v.discount_type.as_deref(),
            Some("percentage"),
            "the cashier can SEE it"
        );
        assert_eq!(v.bill.discount_amount, 200);
        tickets.push(v.id);
    }
    let settle = |ticket: Uuid, extra: serde_json::Value| {
        let mut body = serde_json::json!({ "shift_id": shift, "payment_method": "cash" });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/settle"))
            .insert_header(("Authorization", format!("Bearer {teller_t}")))
            .set_json(&body)
            .to_request()
    };

    // Silence: the waiter's 10% is inherited. 1800 + 14% = 2052.
    let r = test::call_service(
        &app,
        settle(tickets[0], serde_json::json!({ "total_amount": 2052 })),
    )
    .await;
    assert_eq!(r.status(), 200);
    let o: Order = test::read_body_json(r).await;
    assert_eq!((o.discount_amount, o.total_amount), (200, 2052));

    // "none": no discount at all, whatever the waiter set. 2000 + 14% = 2280.
    let r = test::call_service(
        &app,
        settle(
            tickets[1],
            serde_json::json!({ "discount_type": "none", "total_amount": 2280 }),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);
    let o: Order = test::read_body_json(r).await;
    assert_eq!(
        (o.discount_amount, o.discount_type, o.total_amount),
        (0, None, 2280)
    );

    // A cashier's own discount replaces the waiter's outright — never a merge
    // of the waiter's type under the cashier's value. 1500 + 14% = 1710.
    let r = test::call_service(
        &app,
        settle(
            tickets[2],
            serde_json::json!({ "discount_type": "fixed", "discount_value": "500", "total_amount": 1710 }),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);
    let o: Order = test::read_body_json(r).await;
    assert_eq!(o.discount_type.as_deref(), Some("fixed"));
    assert_eq!((o.discount_amount, o.total_amount), (500, 1710));
}

// ── One line off a bill ───────────────────────────────────────

/// "Take the calamari off" — the party changed their mind about one thing.
///
/// The money leaves the bill, the plate leaves the board, and the rest of the
/// round is untouched. Before this existed the only answer was to void the
/// whole bill and re-ring it, which lost the round's timestamps and told the
/// kitchen to start again on food already being made.
#[sqlx::test]
async fn a_line_comes_off_the_bill_and_off_the_board(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 2500).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let _shift = open_shift_row(&pool, branch, waiter).await;
    let table = seed_table(&pool, org, branch, "T7").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "delete").await;
    let t = token(waiter, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "table_id": table,
                "items": [
                    { "menu_item_id": item, "quantity": 2 },
                    { "menu_item_id": item, "quantity": 1 }
                ]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(view.subtotal, 7500, "two plus one, at 25 each");

    // The line to take off, and the kitchen copy that knows it.
    let lines: Vec<(Uuid, i32)> = sqlx::query_as(
        "SELECT id, line_total FROM open_ticket_items WHERE open_ticket_id = $1 \
         ORDER BY line_total DESC",
    )
    .bind(view.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(lines.len(), 2);
    let (big_line, big_total) = lines[0];
    assert_eq!(big_total, 5000);
    let linked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM kitchen_ticket_items WHERE open_ticket_item_id = $1",
    )
    .bind(big_line)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(linked, 1, "the kitchen copy knows which bill line it is");

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/items/{big_line}/void", view.id))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "reason": "customer_request" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(
        view.subtotal, 2500,
        "the bill dropped by exactly what that line added"
    );

    // The void is an event, not a flag: who, when, and why.
    let (voided_by, reason): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT voided_by, void_reason::text FROM open_ticket_items WHERE id = $1")
            .bind(big_line)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(voided_by, Some(waiter));
    assert_eq!(reason.as_deref(), Some("customer_request"));

    // And it left the kitchen's board, while the other line stayed on it.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM kitchen_ticket_items kti \
         JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
         WHERE kt.open_ticket_id = $1 AND kti.voided_at IS NULL",
    )
    .bind(view.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(live, 1, "one plate cancelled, one still cooking");
}

/// A retried drain must not subtract the price twice.
///
/// This is the failure the whole handler is shaped around: the bill's subtotal
/// is a running column, so a second void of the same line would quietly leave
/// the bill short by the price of a plate and nothing downstream would notice
/// until somebody counted the drawer.
#[sqlx::test]
async fn voiding_a_voided_line_does_not_take_the_money_twice(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 2500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T8").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "delete").await;
    let t = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "table_id": table,
                "items": [
                    { "menu_item_id": item, "quantity": 2 },
                    { "menu_item_id": item, "quantity": 1 }
                ]
            }))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(resp).await;
    let line: Uuid = sqlx::query_scalar(
        "SELECT id FROM open_ticket_items WHERE open_ticket_id = $1 AND line_total = 5000",
    )
    .bind(view.id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let void = || {
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/items/{line}/void", view.id))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "reason": "wrong_order" }))
            .to_request()
    };
    assert_eq!(test::call_service(&app, void()).await.status(), 200);
    let resp = test::call_service(&app, void()).await;
    assert_eq!(resp.status(), 200, "a lost-ack retry reads as success");
    let view: OpenTicketView = test::read_body_json(resp).await;
    assert_eq!(view.subtotal, 2500, "and takes nothing the second time");
}

/// A line void needs a reason, like every other void in the system — and a
/// note when the reason is `other`, because "other" on its own says nothing a
/// report can count.
#[sqlx::test]
async fn a_line_void_names_its_reason(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 2500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T9").await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "read").await;
    grant(&pool, "teller", "open_tickets", "delete").await;
    let t = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch, "table_id": table,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(resp).await;
    let line: Uuid =
        sqlx::query_scalar("SELECT id FROM open_ticket_items WHERE open_ticket_id = $1")
            .bind(view.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let post = |body: serde_json::Value| {
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/items/{line}/void", view.id))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&body)
            .to_request()
    };
    assert_eq!(
        test::call_service(&app, post(serde_json::json!({})))
            .await
            .status(),
        400,
        "no reason"
    );
    assert_eq!(
        test::call_service(&app, post(serde_json::json!({ "reason": "other" })))
            .await
            .status(),
        400,
        "'other' with nothing said"
    );
    assert_eq!(
        test::call_service(
            &app,
            post(serde_json::json!({ "reason": "other", "note": "sent back cold" }))
        )
        .await
        .status(),
        200
    );
}

/// A line of a settled bill is an ORDER's line now. Giving money back on it is
/// a refund, and saying otherwise would let a paid sale quietly shrink.
#[sqlx::test]
async fn a_settled_bill_has_no_lines_to_void(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 2500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    let table = seed_table(&pool, org, branch, "T10").await;
    seed_cash_method(&pool, org).await;
    for a in ["create", "read", "update", "delete"] {
        grant(&pool, "teller", "open_tickets", a).await;
    }
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        grant(&pool, "teller", r, a).await;
    }
    let t = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "branch_id": branch, "table_id": table,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    let view: OpenTicketView = test::read_body_json(resp).await;
    let line: Uuid =
        sqlx::query_scalar("SELECT id FROM open_ticket_items WHERE open_ticket_id = $1")
            .bind(view.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/settle", view.id))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({
                "shift_id": shift, "payment_method": "cash", "total_amount": 2850
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200, "settled");

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{}/items/{line}/void", view.id))
            .insert_header(("Authorization", format!("Bearer {t}")))
            .set_json(&serde_json::json!({ "reason": "quality_issue" }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);
}
