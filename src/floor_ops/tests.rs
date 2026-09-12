//! Table occupancy and the transfer waitlist.
//!
//! The invariant under test: a table has at most one live occupant, every
//! occupancy is a ledger row that says who took the table and who ended it, and
//! status is only ever READ -- from `v_table_status`. Parked orders are
//! client-local drafts; only the hold they place on a table has a server
//! presence to test.

use actix_web::{App, test, web};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
use crate::floor_ops::TransferView;
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
/// The derived status -- the one the room is supposed to trust.
async fn table_status(pool: &PgPool, table: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM v_table_status WHERE table_id = $1")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// A table the last party left plates on, spelled the way the backfill spells
/// it: an ended row still carrying its bussing debt.
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
/// `(held_by, open_ticket_id, started_by, started_till_id, ended_by, ended_till_id,
/// end_reason, needs_bussing, cleared_by)` of the table's latest ledger row.
#[allow(clippy::type_complexity)]
async fn latest_row(
    pool: &PgPool,
    table: Uuid,
) -> (
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    Option<String>,
    bool,
    Option<Uuid>,
) {
    sqlx::query_as(
        "SELECT held_by, open_ticket_id, started_by, started_till_id, ended_by, ended_till_id, \
                end_reason, needs_bussing, cleared_by \
           FROM table_occupancies WHERE table_id = $1 \
          ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .bind(table)
    .fetch_one(pool)
    .await
    .unwrap()
}
async fn rows_on(pool: &PgPool, table: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM table_occupancies WHERE table_id = $1")
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn till_of_open_shift(pool: &PgPool, teller: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT till_id FROM shifts WHERE teller_id = $1 AND status = 'open'")
        .bind(teller)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// The `code` a refusal carries -- what the till branches on.
async fn refusal_code(resp: actix_web::dev::ServiceResponse) -> Option<String> {
    let body: serde_json::Value = test::read_body_json(resp).await;
    body["code"].as_str().map(str::to_owned)
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
        ("open_tickets", "delete"), // void is its own rung
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
        ("open_tickets", "delete"), // void is its own rung
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
                // The real `/floor` scope, ops routes included.
                .configure(crate::reservations::routes::configure)
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

/// Fire a ticket, optionally onto a table. Returns the created ticket.
///
/// The only way an order reaches the floor now, so it stands in wherever these
/// tests used to park a held order.
macro_rules! fire_on {
    ($app:expr, $tok:expr, $branch:expr, $item:expr, $table:expr) => {{
        let resp = post_json!(
            $app,
            $tok,
            "/open-tickets",
            serde_json::json!({
                "branch_id": $branch, "table_id": $table,
                "items": [{ "menu_item_id": $item, "quantity": 1 }]
            })
        );
        assert_eq!(resp.status(), 201, "fire ticket");
        let t: OpenTicketView = test::read_body_json(resp).await;
        t
    }};
}

// ── Swap: the atomic two-table exchange ──────────────────────────────────────

#[sqlx::test]
async fn swap_exchanges_two_tickets_atomically(pool: PgPool) {
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

    let a = fire_on!(app, w, branch, item, t1);
    let b = fire_on!(app, w, branch, item, t2);

    // The exchange.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t2 })
    );
    assert_eq!(resp.status(), 200);

    let seat_of = |id: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>("SELECT table_id FROM open_tickets WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        (seat_of(a.id).await, seat_of(b.id).await),
        (Some(t2), Some(t1)),
        "the two parties exchanged tables"
    );
    assert_eq!(table_status(&pool, t1).await, "seated");
    assert_eq!(table_status(&pool, t2).await, "seated");

    // Swapping with an EMPTY table degenerates to a move: the vacated side is
    // FREE, not dirty -- nobody ate there, the party simply moved.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t3 })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(seat_of(b.id).await, Some(t3));
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(table_status(&pool, t3).await, "seated");

    // Two empty tables is a no-op the caller should hear about, rather than a
    // silent success that looks like something happened.
    let t4 = seed_table(&pool, org, branch, None, "T4").await;
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t4 })
    );
    assert_eq!(resp.status(), 400, "both tables empty");
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
    let w = token(waiter, org, UserRole::Waiter);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    // A ticket owns T1.
    let a = fire_on!(app, w, branch, item, t1);
    let _ = a;

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

    // The interactive move onto an OCCUPIED table is a loud 409: an
    // interactive action, unlike an offline fire, can be told 'no'.
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
    // The ledger says who seated it: the waiter, from no till (a handheld
    // opens no shift).
    let row = latest_row(&pool, t1).await;
    assert_eq!(
        (row.0.as_str(), row.1, row.2, row.3),
        ("ticket", Some(ticket.id), Some(waiter), None)
    );

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
    // ...and who ended it, from which till, and why.
    let row = latest_row(&pool, t1).await;
    assert_eq!(row.4, Some(teller), "ended by the cashier");
    assert_eq!(
        row.5,
        Some(till_of_open_shift(&pool, teller).await),
        "at their till"
    );
    assert_eq!((row.6.as_deref(), row.7), (Some("settled"), true));
    // Until the column is dropped, the trigger keeps it saying the same thing.
    let projected: String = sqlx::query_scalar("SELECT status FROM branch_tables WHERE id = $1")
        .bind(t1)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        projected, "dirty",
        "the legacy column is a projection of the ledger"
    );

    // Only a human clearing it makes it available again — no server can see
    // that the plates are gone.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/clear"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(
        latest_row(&pool, t1).await.8,
        Some(teller),
        "clearing is a recorded human act"
    );

    // Clearing again is idempotent: a double-tap on the POS prompt is not an
    // error, and treating it as one would teach staff to ignore errors.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/clear"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
}

/// A till's HOLD on a table syncs, even though the parked order does not.
///
/// A held order is device-local by design — its lines and its money never
/// leave the till, and only the sale it becomes is pushed. But the TABLE is a
/// fact about the room: without this the dashboard's floor, and every other
/// till, were told a table with somebody's order waiting on it was free, and
/// the next party got seated on top of it.
#[sqlx::test]
async fn a_till_can_hold_a_table_for_its_own_parked_order(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    assert_eq!(table_status(&pool, t1).await, "free");

    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(
        table_status(&pool, t1).await,
        "seated",
        "the room now knows the table is taken"
    );

    // Holding again is idempotent: a replayed op after a reconnect, and the
    // ordinary double-tap. Treating either as an error teaches staff to ignore
    // errors.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);

    // And giving it back frees it.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/release"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "free");
}

/// A hold never takes a table a ticket is on, and never frees one either.
#[sqlx::test]
async fn a_hold_cannot_take_or_free_a_table_a_ticket_owns(pool: PgPool) {
    grant_defaults(&pool).await;
    // The defaults give a teller everything but firing a ticket, which is the
    // waiter's job — here the teller stands in for the party being seated.
    grant(&pool, "teller", "open_tickets", "create").await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    // A real party, seated by a ticket.
    let resp = post_json!(
        app,
        t,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t1,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    assert_eq!(table_status(&pool, t1).await, "seated");

    // A hold may not park on top of them.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(
        resp.status(),
        409,
        "somebody is sitting there — the hold is refused, not layered on"
    );
    assert_eq!(
        refusal_code(resp).await.as_deref(),
        Some("TABLE_OCCUPIED"),
        "and the till is told why, in a word it can branch on"
    );

    // And releasing must not strand the ticket by freeing its table. It says
    // OK — the hold is gone either way, which is all the caller claimed — but
    // the table stays the ticket's.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/release"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(
        table_status(&pool, t1).await,
        "seated",
        "the ticket still owns its table"
    );
}

/// Releasing never launders a table that is waiting to be bussed.
#[sqlx::test]
async fn releasing_a_hold_leaves_a_dirty_table_dirty(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    seed_dirty(&pool, t1).await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/release"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(
        table_status(&pool, t1).await,
        "dirty",
        "only a person says the plates are gone"
    );

    // Nor does a new hold launder it: the plates are somebody's problem first.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 409);
    assert_eq!(refusal_code(resp).await.as_deref(), Some("TABLE_DIRTY"));
    assert_eq!(table_status(&pool, t1).await, "dirty");
}

/// A hold has an owner or it does not exist. The ledger names the hand and the
/// till, a second till is refused with a reason it can act on, and the same
/// till re-holding -- the double-tap, or the next shift on the same drawer --
/// is the same hold, not a fight over the table.
#[sqlx::test]
async fn a_hold_is_owned_by_the_till_that_placed_it(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let morning = seed_user(&pool, org, "teller").await;
    let afternoon = seed_user(&pool, org, "teller").await;
    let handheld = seed_user(&pool, org, "waiter").await;
    let shift = open_shift_row(&pool, branch, morning).await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let app = app!(pool);
    let m = token(morning, org, UserRole::Teller);
    let a = token(afternoon, org, UserRole::Teller);
    let h = token(handheld, org, UserRole::Waiter);

    let resp = post_json!(
        app,
        m,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    let till = till_of_open_shift(&pool, morning).await;
    let row = latest_row(&pool, t1).await;
    assert_eq!(
        (row.0.as_str(), row.1, row.2, row.3),
        ("party", None, Some(morning), Some(till)),
        "a bare hold, owned by the morning teller at their till"
    );

    // Another hand, another till (none, for a handheld): refused, with a code.
    let resp = post_json!(
        app,
        h,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 409, "a second party cannot be parked on top");
    assert_eq!(refusal_code(resp).await.as_deref(), Some("TABLE_HELD"));
    assert_eq!(rows_on(&pool, t1).await, 1, "and nothing was written");

    // Shift handover on the same drawer: the afternoon teller inherits the
    // parked draft, and re-holding its table is a yes, not a new row.
    sqlx::query("UPDATE shifts SET status = 'closed', closed_at = now() WHERE id = $1")
        .bind(shift)
        .execute(&pool)
        .await
        .unwrap();
    open_shift_row(&pool, branch, afternoon).await;
    assert_eq!(
        till_of_open_shift(&pool, afternoon).await,
        till,
        "same till"
    );
    let resp = post_json!(
        app,
        a,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(rows_on(&pool, t1).await, 1);

    // Whoever checks the draft out releases it, and the ledger says who did.
    let resp = post_json!(
        app,
        a,
        &format!("/floor/tables/{t1}/release"),
        serde_json::json!({ "branch_id": branch, "bus": true })
    );
    assert_eq!(resp.status(), 200);
    let row = latest_row(&pool, t1).await;
    assert_eq!((row.4, row.5), (Some(afternoon), Some(till)));
    assert_eq!((row.6.as_deref(), row.7), (Some("released"), true));
    assert_eq!(table_status(&pool, t1).await, "dirty");
}

// ── Transfer waitlist ────────────────────────────────────────────────────────

#[sqlx::test]
async fn transfer_waitlist_lifecycle(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    let outside = seed_section(&pool, org, branch, "Outside").await;
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let t_out = seed_table(&pool, org, branch, Some(outside), "O1").await;
    let t_in = seed_table(&pool, org, branch, Some(inside), "I1").await;

    // A party seated outside wants "anywhere inside".
    let a = fire_on!(app, w, branch, item, t_out).id;
    let wish = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
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
    // The queue labels a party by their ticket reference — the thing staff
    // actually say out loud.
    assert!(
        view.occupant_label
            .as_deref()
            .is_some_and(|l| l.starts_with("T-")),
        "expected a ticket ref, got {:?}",
        view.occupant_label
    );

    // Retrying the SAME create dedups; a SECOND wish for the party conflicts.
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 200);
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": Uuid::new_v4(), "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
            "target_section_id": inside
        })
    );
    assert_eq!(resp.status(), 409);
    assert_eq!(refusal_code(resp).await.as_deref(), Some("TRANSFER_EXISTS"));

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
    let ta: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id=$1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ta, Some(t_in));
    assert_eq!(table_status(&pool, t_out).await, "free");
    assert_eq!(table_status(&pool, t_in).await, "seated");
    let left = latest_row(&pool, t_out).await;
    assert_eq!(
        (left.6.as_deref(), left.4),
        (Some("moved"), Some(teller)),
        "O1's row ended `moved`, by the host"
    );

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
    assert_eq!(
        refusal_code(resp).await.as_deref(),
        Some("TRANSFER_FULFILLED")
    );
}

#[sqlx::test]
async fn assigning_into_the_wished_section_autofulfills(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let t = token(teller, org, UserRole::Teller);
    let w = token(waiter, org, UserRole::Waiter);
    let item = seed_menu_item(&pool, org, 1000).await;
    shift_row(&pool, branch, teller).await;
    let inside = seed_section(&pool, org, branch, "Inside").await;
    let t_in = seed_table(&pool, org, branch, Some(inside), "I1").await;

    // A table-less ("waiting at the door") order queues for inside.
    let a = fire_on!(app, w, branch, item, serde_json::Value::Null).id;
    let wish = Uuid::new_v4();
    let resp = post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": a,
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
        &format!("/floor/transfers/{wish}/fulfill"),
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

    // Voiding the order cancels a waiting wish: the party left the floor, so
    // the queue must not keep holding a place for them.
    let b = fire_on!(app, w, branch, item, serde_json::Value::Null).id;
    let wish_b = Uuid::new_v4();
    post_json!(
        app,
        t,
        "/floor/transfers",
        serde_json::json!({
            "id": wish_b, "branch_id": branch, "occupant_kind": "open_ticket", "occupant_id": b,
            "target_section_id": inside
        })
    );
    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{b}/void"),
        serde_json::json!({ "reason": "left" })
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
async fn replay_applies_a_queued_swap_and_honours_role_boundaries(pool: PgPool) {
    // Offline table moves still replay through the same core as the live route.
    // Parking is NOT here: a parked order is a client-local draft, so it has
    // nothing to replay -- the offline path for the commonest POS action is now
    // no path at all.
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

    let a = fire_on!(app, w, branch, item, t1);

    // A queued offline swap replays and applies.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "swap_tables", "teller_id": teller,
            "request": { "branch_id": branch, "table_a": t1, "table_b": t2 }
        })
    );
    assert_eq!(resp.status(), 200, "queued swap replays");
    let seat: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id=$1")
        .bind(a.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(seat, Some(t2), "the party moved to the empty table");
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(table_status(&pool, t2).await, "seated");

    // Replaying the same op again (a lost ack) must not bounce the party back.
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

    // Replay must check the op's EMBEDDED actor, not the caller's token, or a
    // revoked permission could be bypassed by queueing the op offline.
    let stranger = seed_user(&pool, org, "waiter").await;
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) \
         VALUES ($1, 'open_tickets'::permission_resource, 'update'::permission_action, false)",
    )
    .bind(stranger)
    .execute(&pool)
    .await
    .unwrap();
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({
            "op": "swap_tables", "teller_id": stranger,
            "request": { "branch_id": branch, "table_a": t1, "table_b": t2 }
        })
    );
    assert_eq!(
        resp.status(),
        403,
        "a revoked permission is not bypassable by replaying offline"
    );
}

// ── Table state (POS operational edits: status walk + zone move) ─────────────

/// `main.rs` once mounted TWO `web::scope("/floor")` — geometry from
/// `reservations::routes`, then swap/clear/transfers from `floor_ops::routes`.
/// actix hands a prefix to the FIRST scope that matches and never falls
/// through, so the second one was dead: every path in it 404'd in production
/// (405 for `/tables/swap`, which `/tables/{id}` matched) while every test
/// passed, because no test ever mounted both.
///
/// They are one scope now. This asserts the whole of it answers — geometry AND
/// operations — through the single `configure` main.rs calls.
#[sqlx::test]
async fn the_whole_floor_scope_is_reachable(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let tok = token(teller, org, UserRole::Teller);
    let table = seed_table(&pool, org, branch, None, "T1").await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(BranchEventHub::new()))
            .configure(crate::reservations::routes::configure),
    )
    .await;

    // Geometry.
    let r = get_req!(app, tok, &format!("/floor/sections?branch_id={branch}"));
    assert_eq!(r.status(), 200, "/floor/sections");
    let r = get_req!(app, tok, &format!("/floor/tables?branch_id={branch}"));
    assert_eq!(r.status(), 200, "/floor/tables");

    // Cross-table operations: the half a shadowed prefix used to eat.
    let r = get_req!(app, tok, &format!("/floor/transfers?branch_id={branch}"));
    assert_eq!(r.status(), 200, "/floor/transfers");

    // A table's own history. `{id}` also matches the literal `swap`, so this
    // has to stay reachable as its own shape rather than being eaten by it.
    let r = get_req!(app, tok, &format!("/floor/tables/{table}/history"));
    assert_eq!(r.status(), 200, "/floor/tables/{{id}}/history");
    let r = post_json!(
        app,
        tok,
        &format!("/floor/tables/{table}/clear"),
        serde_json::json!({})
    );
    assert_ne!(r.status(), 404, "/floor/tables/{{id}}/clear");
    let r = post_json!(
        app,
        tok,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": table, "table_b": table })
    );
    assert_ne!(r.status(), 404, "/floor/tables/swap");
    assert_ne!(
        r.status(),
        405,
        "/floor/tables/swap: /tables/{{id}} swallowed it"
    );
}

/// Bussing a table is the one floor transition a server cannot observe, and it
/// had no way home. The POS queues it as `clear_table`; `/sync/replay` had no
/// such op, so the envelope failed to deserialize, came back 400, and the POS
/// classifies a 400 as permanently dead — the op was dropped, and the next
/// floor pull put the table back to `dirty`. Tables stayed dirty forever.
///
/// The shipped v0.2.0 core sends `request: {}` (it has never had a branch to
/// put there), so that exact envelope is what this replays.
#[sqlx::test]
async fn replay_clears_a_bussed_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let shift = open_shift_row(&pool, branch, teller).await;
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
    let table = seed_table(&pool, org, branch, None, "T1").await;

    // Seat a party, then settle: checkout busses the table, it does not free it.
    let tk = fire_on!(app, w, branch, item, table);
    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{}/settle", tk.id),
        serde_json::json!({ "shift_id": shift, "payment_method": "cash" })
    );
    assert_eq!(resp.status(), 200, "settle");
    assert_eq!(table_status(&pool, table).await, "dirty");

    // The envelope the POS actually queues — empty request, no branch.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "clear_table", "teller_id": teller, "table_id": table, "request": {} })
    );
    assert_eq!(resp.status(), 200, "queued clear replays");
    assert_eq!(table_status(&pool, table).await, "free");

    // A lost ack replays it again; `free` -> `free` is a no-op, not a 409.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "clear_table", "teller_id": teller, "table_id": table, "request": {} })
    );
    assert_eq!(resp.status(), 200, "replaying a clear is idempotent");
    assert_eq!(table_status(&pool, table).await, "free");
}

/// The offline path: a till parks an order on a table with no network, and the
/// occupancy reaches the floor when it drains — through the same code the live
/// route uses, with the branch read off the table.
#[sqlx::test]
async fn a_queued_hold_reaches_the_floor_when_it_drains(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "hold_table", "teller_id": teller, "table_id": t1, "request": {} })
    );
    assert_eq!(resp.status(), 200, "queued hold replays");
    assert_eq!(table_status(&pool, t1).await, "seated");

    // A lost ack replays it; `seated` -> `seated` is a yes, not a 409.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "hold_table", "teller_id": teller, "table_id": t1, "request": {} })
    );
    assert_eq!(resp.status(), 200, "replaying a hold is idempotent");

    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "release_table", "teller_id": teller, "table_id": t1, "request": {} })
    );
    assert_eq!(resp.status(), 200, "queued release replays");
    assert_eq!(table_status(&pool, t1).await, "free");

    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "release_table", "teller_id": teller, "table_id": t1, "request": {} })
    );
    assert_eq!(resp.status(), 200, "replaying a release is idempotent");
}

/// The fork the till makes locally when a parked order ends: discarded means
/// nobody ever sat and the table goes back to the room; checked out means the
/// party ate, and the table waits for a human with a cloth.
#[sqlx::test]
async fn a_released_hold_lands_dirty_when_the_party_ate(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);

    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/release"),
        serde_json::json!({ "branch_id": branch, "bus": true })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(
        table_status(&pool, t1).await,
        "dirty",
        "the plates are still on it"
    );

    // And a release with no `bus` cannot undo that: clearing is a person's job.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/release"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "dirty");
}

/// Neither op may cross an org boundary; like a clear, the table is what
/// resolves the branch, so the table is what has to be checked.
#[sqlx::test]
async fn replay_hold_cannot_reach_another_orgs_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);

    let other = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other).await;
    let their_table = seed_table(&pool, other, other_branch, None, "T1").await;

    for op in ["hold_table", "release_table"] {
        let resp = post_json!(
            app,
            t,
            "/sync/replay",
            serde_json::json!({ "op": op, "teller_id": teller, "table_id": their_table, "request": {} })
        );
        assert!(
            matches!(resp.status().as_u16(), 403 | 404),
            "cross-org {op} rejected, got {}",
            resp.status()
        );
        assert_eq!(table_status(&pool, their_table).await, "free");
    }
}

/// A clear must never cross an org boundary, even though the op carries no
/// branch of its own: the table is what resolves to one.
#[sqlx::test]
async fn replay_clear_cannot_reach_another_orgs_table(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let t = token(teller, org, UserRole::Teller);

    let other = seed_org(&pool).await;
    let other_branch = seed_branch(&pool, other).await;
    let their_table = seed_table(&pool, other, other_branch, None, "T1").await;

    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "clear_table", "teller_id": teller, "table_id": their_table, "request": {} })
    );
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "cross-org clear rejected, got {}",
        resp.status()
    );
}

/// A table's takings come from a join nobody was reading: a settled bill
/// carries `orders.open_ticket_id` and the ticket carries `table_id`. Until
/// this endpoint a shop could look at a room full of tables and not answer
/// which of them actually earns.
///
/// Only SETTLED bills count toward money and covers. An open bill has not
/// finished and a voided one took nothing; folding either in would flatter a
/// table that lost money.
#[sqlx::test]
async fn table_history_counts_only_what_the_table_actually_took(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    grant_defaults(&pool).await;
    let tok = token(teller, org, UserRole::Teller);
    let table = seed_table(&pool, org, branch, None, "T1").await;

    let opened = chrono::Utc::now() - chrono::Duration::hours(3);
    let closed = chrono::Utc::now() - chrono::Duration::hours(2);

    // A settled bill: two covers, 5000.
    let settled: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets (org_id, branch_id, table_id, ticket_ref, opened_by, \
             guest_count, status, opened_at, settled_at, settled_by) \
         VALUES ($1,$2,$3,'T-1',$4,2,'settled',$5,$6,$4) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .bind(table)
    .bind(teller)
    .bind(opened)
    .bind(closed)
    .fetch_one(&pool)
    .await
    .unwrap();
    seed_order_for_ticket(&pool, org, branch, teller, settled, 5000, false).await;

    // A VOIDED bill on the same table: it took nothing.
    let voided: Uuid = sqlx::query_scalar(
        "INSERT INTO open_tickets (org_id, branch_id, table_id, ticket_ref, opened_by, \
             guest_count, status, opened_at, voided_at) \
         VALUES ($1,$2,$3,'T-2',$4,9,'voided',$5,$6) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .bind(table)
    .bind(teller)
    .bind(opened)
    .bind(closed)
    .fetch_one(&pool)
    .await
    .unwrap();
    seed_order_for_ticket(&pool, org, branch, teller, voided, 9999, true).await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(BranchEventHub::new()))
            .configure(crate::reservations::routes::configure),
    )
    .await;

    let r = get_req!(app, tok, &format!("/floor/tables/{table}/history"));
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = test::read_body_json(r).await;

    assert_eq!(body["label"], "T1");
    assert_eq!(
        body["sittings"].as_array().unwrap().len(),
        2,
        "both sittings are listed — the history shows what happened"
    );
    assert_eq!(body["settled_count"], 1, "only the settled bill counts");
    assert_eq!(body["total_minor"], 5000, "the voided bill took nothing");
    assert_eq!(body["covers"], 2, "and brought nobody");
    assert_eq!(body["average_bill_minor"], 5000);
    assert_eq!(
        body["average_minutes"], 60,
        "opened_at -> settled_at, not to now"
    );
}

/// A settled sale against an open ticket, so the history has something to
/// join to. `voided` writes the `voided_at` that must keep it out of takings.
async fn seed_order_for_ticket(
    pool: &PgPool,
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
    ticket: Uuid,
    total: i32,
    voided: bool,
) {
    // One open shift per teller, so reuse this teller's if it already has one.
    let shift: Uuid = sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id, opening_cash, status) \
         VALUES ($1,$2,0,'open') \
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_optional(pool)
    .await
    .unwrap()
    .unwrap_or(
        sqlx::query_scalar("SELECT id FROM shifts WHERE teller_id = $1 AND status = 'open'")
            .bind(teller)
            .fetch_one(pool)
            .await
            .unwrap(),
    );
    let _ = org;
    sqlx::query(
        "INSERT INTO orders (branch_id, shift_id, teller_id, order_number, order_ref, \
             status, payment_method, subtotal, total_amount, open_ticket_id, \
             voided_at, voided_by) \
         VALUES ($1,$2,$3,$7,'O-' || $7::text, \
                 CASE WHEN $6::timestamptz IS NULL THEN 'completed' ELSE 'voided' END::order_status, \
                 'cash',$4,$4,$5,$6, \
                 CASE WHEN $6::timestamptz IS NULL THEN NULL ELSE $3::uuid END)",
    )
    .bind(branch)
    .bind(shift)
    .bind(teller)
    .bind(total)
    .bind(ticket)
    .bind(if voided {
        Some(chrono::Utc::now())
    } else {
        None
    })
    .bind(if voided { 2_i32 } else { 1_i32 })
    .execute(pool)
    .await
    .unwrap();
}

/// A party seated with nothing ordered carries the till's seating stamp: the
/// floor shows it, the bill that takes over the hold inherits it, and the sale
/// the bill settles into remembers the table, the stamp and the covers.
#[sqlx::test]
async fn the_seating_clock_survives_from_hold_to_sale(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let shift = open_shift_row(&pool, branch, teller).await;
    seed_cash_method(&pool, org).await;
    grant_defaults(&pool).await;
    for (resource, action) in [
        ("orders", "create"),
        ("payments", "create"),
        ("open_tickets", "create"),
        ("kitchen_orders", "read"),
        ("kitchen_orders", "update"),
    ] {
        grant(&pool, "teller", resource, action).await;
    }
    let t = token(teller, org, UserRole::Teller);
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;

    // Seated offline 20 minutes ago; the op drains now.
    let sat = chrono::Utc::now() - chrono::Duration::minutes(20);
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "hold_table", "teller_id": teller, "table_id": t1,
                            "request": { "seated_at": sat.to_rfc3339() } })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(table_status(&pool, t1).await, "seated");
    let shown: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT seated_at FROM v_table_status WHERE table_id = $1")
            .bind(t1)
            .fetch_one(&pool)
            .await
            .unwrap();
    let shown = shown.expect("a seated table has a clock");
    assert!(
        (shown - sat).num_seconds().abs() <= 1,
        "the floor shows the seating"
    );

    // A stamp from the future is clamped to the hold itself.
    let future = chrono::Utc::now() + chrono::Duration::hours(3);
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "hold_table", "teller_id": teller, "table_id": t2,
                            "request": { "seated_at": future.to_rfc3339() } })
    );
    assert_eq!(resp.status(), 200);
    let clamped: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT seated_at FROM v_table_status WHERE table_id = $1")
            .bind(t2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        clamped.unwrap() <= chrono::Utc::now(),
        "never in the future"
    );

    // The first round takes over the hold and keeps the party's clock.
    let resp = post_json!(
        app,
        t,
        "/open-tickets",
        serde_json::json!({
            "branch_id": branch, "table_id": t1, "guest_count": 3,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    );
    assert_eq!(resp.status(), 201);
    let ticket: OpenTicketView = test::read_body_json(resp).await;
    let ticket_seated: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT seated_at FROM open_tickets WHERE id = $1")
            .bind(ticket.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        (ticket_seated.unwrap() - sat).num_seconds().abs() <= 1,
        "the bill inherits the seating"
    );

    let resp = post_json!(
        app,
        t,
        &format!("/open-tickets/{}/settle", ticket.id),
        serde_json::json!({ "shift_id": shift, "payment_method": "cash" })
    );
    assert_eq!(resp.status(), 200);
    let (table_id, seated_at, covers): (
        Option<Uuid>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<i32>,
    ) = sqlx::query_as("SELECT table_id, seated_at, covers FROM orders WHERE open_ticket_id = $1")
        .bind(ticket.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(table_id, Some(t1));
    assert!((seated_at.unwrap() - sat).num_seconds().abs() <= 1);
    assert_eq!(covers, Some(3));

    // And the history measures dwell from the seating, not the bill.
    let r = get_req!(app, t, &format!("/floor/tables/{t1}/history"));
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = test::read_body_json(r).await;
    assert!(body["average_minutes"].as_i64().unwrap() >= 19);
}

// ── Moves carry parties with no bill yet ─────────────────────────────────────

/// `(party_size, seated_at)` of the live row on `table`.
async fn live_party(
    pool: &PgPool,
    table: Uuid,
) -> Option<(String, Option<i16>, Option<chrono::DateTime<chrono::Utc>>)> {
    sqlx::query_as(
        "SELECT held_by, party_size, COALESCE(seated_at, started_at) FROM table_occupancies \
          WHERE table_id = $1 AND ended_at IS NULL",
    )
    .bind(table)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_hold_records_its_covers(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch, "party_size": 4 })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live_party(&pool, t1).await.unwrap().1, Some(4));

    // A recount on the same hold corrects it.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t1}/hold"),
        serde_json::json!({ "branch_id": branch, "party_size": 5 })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live_party(&pool, t1).await.unwrap().1, Some(5));

    // A queued hold carries it through replay too; nonsense is not recorded.
    let resp = post_json!(
        app,
        t,
        "/sync/replay",
        serde_json::json!({ "op": "hold_table", "teller_id": teller, "table_id": t2,
                            "request": { "party_size": 3 } })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live_party(&pool, t2).await.unwrap().1, Some(3));
}

#[sqlx::test]
async fn swap_exchanges_two_parties_with_no_bill(pool: PgPool) {
    grant_defaults(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let t1 = seed_table(&pool, org, branch, None, "T1").await;
    let t2 = seed_table(&pool, org, branch, None, "T2").await;
    let t3 = seed_table(&pool, org, branch, None, "T3").await;
    let app = app!(pool);
    let t = token(teller, org, UserRole::Teller);

    let early = chrono::Utc::now() - chrono::Duration::minutes(40);
    for (table, size) in [(t1, 2), (t2, 6)] {
        let resp = post_json!(
            app,
            t,
            &format!("/floor/tables/{table}/hold"),
            serde_json::json!({ "branch_id": branch, "party_size": size,
                                "seated_at": if size == 2 { Some(early) } else { None } })
        );
        assert_eq!(resp.status(), 200);
    }
    let clock_1 = live_party(&pool, t1).await.unwrap().2;
    let clock_2 = live_party(&pool, t2).await.unwrap().2;

    // Move the party on T1 to the empty T3: the table follows the party.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t3 })
    );
    assert_eq!(resp.status(), 200, "a party with no bill is an occupant");
    assert_eq!(table_status(&pool, t1).await, "free");
    assert_eq!(table_status(&pool, t3).await, "seated");
    let landed = live_party(&pool, t3).await.unwrap();
    assert_eq!(landed.1, Some(2), "covers travel");
    assert_eq!(landed.2, clock_1, "the seating clock travels");
    let moved: String = sqlx::query_scalar(
        "SELECT end_reason FROM table_occupancies WHERE table_id = $1 AND ended_at IS NOT NULL",
    )
    .bind(t1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(moved, "moved");

    // Swap the two parties.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t3, "table_b": t2 })
    );
    assert_eq!(resp.status(), 200);
    assert_eq!(live_party(&pool, t2).await.unwrap().1, Some(2));
    assert_eq!(live_party(&pool, t3).await.unwrap().1, Some(6));
    assert_eq!(live_party(&pool, t3).await.unwrap().2, clock_2);

    // The till that placed the hold still owns it where it landed.
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t2}/hold"),
        serde_json::json!({ "branch_id": branch })
    );
    assert_eq!(
        resp.status(),
        200,
        "re-holding your own moved hold is a yes"
    );

    // Nobody lands on plates.
    let t4 = seed_table(&pool, org, branch, None, "T4").await;
    seed_dirty(&pool, t4).await;
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t2, "table_b": t4 })
    );
    assert_eq!(resp.status(), 409);
    assert_eq!(refusal_code(resp).await.as_deref(), Some("TABLE_DIRTY"));
    assert_eq!(
        live_party(&pool, t2).await.unwrap().1,
        Some(2),
        "nothing moved"
    );
}

#[sqlx::test]
async fn a_bill_swaps_with_a_waiting_party_instead_of_wiping_it(pool: PgPool) {
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

    let bill = fire_on!(app, w, branch, item, t1);
    let resp = post_json!(
        app,
        t,
        &format!("/floor/tables/{t2}/hold"),
        serde_json::json!({ "branch_id": branch, "party_size": 3 })
    );
    assert_eq!(resp.status(), 200);

    // The interactive single-ticket move refuses to land on them...
    let resp = test::call_service(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/open-tickets/{}/table", bill.id))
            .insert_header(("Authorization", format!("Bearer {w}")))
            .set_json(&serde_json::json!({ "table_id": t2 }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 409);
    assert_eq!(refusal_code(resp).await.as_deref(), Some("TABLE_HELD"));

    // ...and the swap exchanges them.
    let resp = post_json!(
        app,
        t,
        "/floor/tables/swap",
        serde_json::json!({ "branch_id": branch, "table_a": t1, "table_b": t2 })
    );
    assert_eq!(resp.status(), 200);
    let seat: Option<Uuid> = sqlx::query_scalar("SELECT table_id FROM open_tickets WHERE id=$1")
        .bind(bill.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(seat, Some(t2), "the bill moved");
    let on_1 = live_party(&pool, t1)
        .await
        .expect("the waiting party was kept");
    assert_eq!((on_1.0.as_str(), on_1.1), ("party", Some(3)));
    assert_eq!(live_party(&pool, t2).await.unwrap().0, "ticket");
    let ended_seated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM table_occupancies WHERE end_reason = 'seated' AND branch_id = $1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(ended_seated, 0, "no party was swallowed by the bill");
}
