//! `/sync/replay` authorization: attribution is the role's only job, and the
//! permission TABLE answers what a queued op may do — the same table, through
//! the same resolver, as the live route. Each test here is one way the old
//! hard-coded role → op match used to disagree with that table.

use actix_web::{App, test, web};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;
use madar_rust::tickets::OpenTicketView;

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
async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .fetch_one(pool)
    .await
    .unwrap()
}
/// A role default, as the dashboard's role matrix would set it.
async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) \
         VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) \
         ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}
/// A per-user override, as the dashboard's user matrix would set it.
async fn override_for(pool: &PgPool, user: Uuid, resource: &str, action: &str, granted: bool) {
    sqlx::query(
        "INSERT INTO permissions (user_id, resource, action, granted) \
         VALUES ($1, $2::permission_resource, $3::permission_action, $4)",
    )
    .bind(user)
    .bind(resource)
    .bind(action)
    .bind(granted)
    .execute(pool)
    .await
    .unwrap();
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::tickets::routes::configure)
                .configure(madar_rust::kitchen::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

async fn replay(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    bearer: &str,
    op: &serde_json::Value,
) -> actix_web::dev::ServiceResponse {
    test::call_service(
        app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(op)
            .to_request(),
    )
    .await
}

/// Fire a one-line ticket by replay, attributed to `actor`; returns the view.
async fn fire_by_replay(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    bearer: &str,
    actor: Uuid,
    branch: Uuid,
    item: Uuid,
) -> actix_web::dev::ServiceResponse {
    let fire = serde_json::json!({
        "op": "fire_open_ticket",
        "teller_id": actor,
        "request": {
            "branch_id": branch,
            "idempotency_key": Uuid::new_v4(),
            "round_idempotency_key": Uuid::new_v4(),
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        }
    });
    replay(app, bearer, &fire).await
}

/// The owner ruled that a branch manager may work the till. The seeder
/// promised it and the replay gate refused it: the role → op table admitted
/// only teller / waiter / kitchen, so every op a manager queued came back
/// "Replay actor may not perform this operation". With the SEEDED defaults —
/// no test-local grants — a manager's queued fire and void both land, under
/// the manager's name.
#[sqlx::test]
async fn a_branch_manager_can_work_the_till_through_the_queue(pool: PgPool) {
    let app = app!(pool);
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    // The device draining the queue is signed in as the teller; the op was the
    // manager's.
    let bearer = token(teller, org, UserRole::Teller);

    let r = fire_by_replay(&app, &bearer, manager, branch, item).await;
    assert_eq!(r.status(), 201, "a manager's queued fire is admitted");
    let view: OpenTicketView = test::read_body_json(r).await;
    assert_eq!(view.opened_by, manager, "attributed to the manager");

    let void = serde_json::json!({
        "op": "void_open_ticket",
        "teller_id": manager,
        "ticket_id": view.id,
        "request": { "reason": "wrong_order" }
    });
    let r = replay(&app, &bearer, &void).await;
    assert_eq!(r.status(), 200, "a manager's queued void is admitted");
    let voided_by: Option<Uuid> =
        sqlx::query_scalar("SELECT voided_by FROM open_tickets WHERE id = $1")
            .bind(view.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        voided_by,
        Some(manager),
        "the void carries the manager's name"
    );
}

/// A grant made in the dashboard has to work offline. A waiter granted
/// `kitchen_orders:update` per user could bump a line live, and the queued
/// bump was refused because the role table said "kitchen or teller". The
/// table decides now: with the grant the bump lands, without it the same
/// table turns it away.
#[sqlx::test]
async fn a_dashboard_grant_works_through_the_queue(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let teller = seed_user(&pool, org, "teller").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "waiter", "open_tickets", "create").await;
    let bearer = token(teller, org, UserRole::Teller);

    let r = fire_by_replay(&app, &bearer, waiter, branch, item).await;
    assert_eq!(r.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(r).await.id;
    let kitchen_item: Uuid = sqlx::query_scalar(
        "SELECT kti.id FROM kitchen_ticket_items kti \
         JOIN kitchen_tickets kt ON kt.id = kti.kitchen_ticket_id \
         WHERE kt.source_type = 'open_ticket' AND kt.source_id = $1",
    )
    .bind(ticket_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let bump = serde_json::json!({
        "op": "bump_kitchen_item", "teller_id": waiter, "item_id": kitchen_item
    });

    // No grant: the TABLE says no. (Not a role list — the message is the
    // permission checker's, naming the resource.)
    let r = replay(&app, &bearer, &bump).await;
    assert_eq!(r.status(), 403, "a waiter with no bump grant is refused");
    let body = test::read_body(r).await;
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("kitchen_orders"),
        "refused by the permission table, not by a role match: {body:?}"
    );

    // The dashboard grants this one waiter the bump → the queued bump lands.
    override_for(&pool, waiter, "kitchen_orders", "update", true).await;
    let r = replay(&app, &bearer, &bump).await;
    assert_eq!(r.status(), 204, "the per-user grant is honoured offline");
    let bumped_by: Option<Uuid> =
        sqlx::query_scalar("SELECT bumped_by FROM kitchen_ticket_items WHERE id = $1")
            .bind(kitchen_item)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bumped_by, Some(waiter));
}

/// Voiding is its own rung. `orders:update` / `open_tickets:update` no longer
/// carry it — so a teller who can add to a bill but has had the void grant
/// revoked cannot tear the bill up by queueing the void, even though the role
/// default says tellers may.
#[sqlx::test]
async fn a_revoked_void_does_not_get_through_by_being_queued(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    let bearer = token(teller, org, UserRole::Teller);

    let r = fire_by_replay(&app, &bearer, teller, branch, item).await;
    assert_eq!(r.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(r).await.id;
    let void = serde_json::json!({
        "op": "void_open_ticket",
        "teller_id": teller,
        "ticket_id": ticket_id,
        "request": { "reason": "customer_request" }
    });

    // `update` alone — the old shared rung — does not void any more.
    let r = replay(&app, &bearer, &void).await;
    assert_eq!(r.status(), 403, "adding to a bill is not tearing it up");

    // The role may void, but THIS teller has had it taken away.
    grant(&pool, "teller", "open_tickets", "delete").await;
    override_for(&pool, teller, "open_tickets", "delete", false).await;
    let r = replay(&app, &bearer, &void).await;
    assert_eq!(r.status(), 403, "a per-user revocation holds offline");
    let status: String = sqlx::query_scalar("SELECT status::text FROM open_tickets WHERE id = $1")
        .bind(ticket_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "open", "the bill is untouched");

    // Same for a counter sale: `orders:update` is not the void rung either.
    // The check runs before the target is even looked up, so a revoked actor
    // is turned away without the order existing.
    grant(&pool, "teller", "orders", "update").await;
    let void_order = serde_json::json!({
        "op": "void_order",
        "teller_id": teller,
        "order_id": Uuid::new_v4(),
        "request": { "reason": "wrong_order" }
    });
    let r = replay(&app, &bearer, &void_order).await;
    assert_eq!(r.status(), 403, "orders:update does not void");
}

/// Accept and flag (PERMISSIONS_ARCHITECTURE §4.4.5, and the owner's binding
/// decision that offline acts failing the server re-check are accepted and
/// flagged, never rejected).
///
/// Cash left the drawer while the shop was offline. By the time the op reaches
/// us the money is gone and the note is written, so refusing the op cannot
/// un-take it — it only loses the record and leaves the drawer short at close.
/// The op is applied and a row goes to the owner's queue instead.
#[sqlx::test]
async fn a_revoked_money_op_is_accepted_and_flagged(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let till = open_shift_row(&pool, branch, teller).await;
    // The role may move cash; THIS teller has had it taken away since.
    grant(&pool, "teller", "tills", "update").await;
    override_for(&pool, teller, "tills", "update", false).await;
    let bearer = token(teller, org, UserRole::Teller);

    let op = serde_json::json!({
        "op": "cash_movement",
        "teller_id": teller,
        "till_id": till,
        "request": { "amount": -500, "note": "milk run", "client_ref": Uuid::new_v4() }
    });
    let r = replay(&app, &bearer, &op).await;
    assert!(
        r.status().is_success(),
        "the cash already left the drawer: got {:?}",
        r.status()
    );

    let (flag_op, cap, reason, author): (String, String, String, Uuid) = sqlx::query_as(
        "SELECT op, capability, reason, author_id FROM authz_replay_flags WHERE org_id = $1",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .expect("exactly one flag row for the owner");
    assert_eq!(flag_op, "CashMovement");
    assert_eq!(cap, "tills:update");
    assert_eq!(author, teller);
    // The override was written after the op's (absent, so "now") timestamp,
    // so nothing explains the device's belief: it is not a stale snapshot.
    assert_eq!(reason, "unauthorized_offline");

    // And the money really moved — a flag is a notice, not a rollback.
    let moved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM till_cash_movements WHERE till_id = $1")
            .bind(till)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(moved, 1, "the op applied");
}

/// The other half of accept-and-flag: a NON-money op is still refused. Nothing
/// irreversible happened, so refusing a bump the actor may not make loses
/// nothing and keeps the server's answer the same online and offline.
#[sqlx::test]
async fn a_revoked_non_money_op_is_still_rejected(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let _till = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "teller", "kitchen_orders", "update").await;
    override_for(&pool, teller, "kitchen_orders", "update", false).await;
    let bearer = token(teller, org, UserRole::Teller);

    let op = serde_json::json!({
        "op": "bump_kitchen_item",
        "teller_id": teller,
        "item_id": Uuid::new_v4(),
    });
    let r = replay(&app, &bearer, &op).await;
    assert_eq!(r.status(), 403, "a bump is not money");
    let flags: i64 =
        sqlx::query_scalar("SELECT count(*) FROM authz_replay_flags WHERE org_id = $1")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(flags, 0, "a refusal is not a flag");
}

/// Attribution: whose name may go on a replayed write. Anyone who may sign in at
/// a till (`pos.sign_in`) — owners included, by the owner's decision that owners
/// and managers work a till with a PIN — but never a till user of another org,
/// whatever they hold there, and never a disabled account.
#[sqlx::test]
async fn only_a_till_user_of_this_org_can_be_the_author(pool: PgPool) {
    let app = app!(pool);
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let other_org = seed_org(&pool).await;
    let stranger = seed_user(&pool, other_org, "teller").await;
    let _shift = open_shift_row(&pool, branch, teller).await;
    let bearer = token(teller, org, UserRole::Teller);

    let r = fire_by_replay(&app, &bearer, admin, branch, item).await;
    assert!(
        r.status().is_success(),
        "an owner works a till: {}",
        r.status()
    );
    let r = fire_by_replay(&app, &bearer, stranger, branch, item).await;
    assert_eq!(r.status(), 403, "a teller of another org is not ours");

    // And a disabled account, whatever it holds.
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(teller)
        .execute(&pool)
        .await
        .unwrap();
    let r = fire_by_replay(&app, &bearer, teller, branch, item).await;
    assert_eq!(r.status(), 403, "a disabled till user is not attributed");
}

/// A refund queued offline flushes through the same door as a queued sale:
/// attributed to the embedded actor, gated by `refunds:create` in the table,
/// idempotent on `client_ref`, and it must NAME the drawer the money left —
/// there is no "current shift" for an op that happened hours ago. Nothing in
/// the refunds lane could add this arm to the queue; the seam is here.
#[sqlx::test]
async fn a_queued_refund_lands_once_under_its_author(pool: PgPool) {
    let app = app!(pool);
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    let order: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, gen_random_uuid(), 300, 0, 300, 'completed', 1, 'cash', gen_random_uuid()::text) RETURNING id",
    )
    .bind(branch)
    .bind(teller)
    .bind(shift)
    .fetch_one(&pool)
    .await
    .unwrap();
    let bearer = token(teller, org, UserRole::Teller);

    // A replayed refund names its shift; without it there is nothing to
    // charge the money to.
    let nameless = serde_json::json!({
        "op": "refund_order",
        "teller_id": manager,
        "request": { "order_id": order, "amount": 100, "method": "cash", "reason": "quality_issue" }
    });
    let r = replay(&app, &bearer, &nameless).await;
    assert_eq!(r.status(), 400, "a queued refund must name the shift");

    // The manager issued it at the till (owner's ruling: no approval flow),
    // an hour ago; the queue carries the real time and a client_ref.
    let cref = Uuid::new_v4();
    let issued_at = chrono::Utc::now() - chrono::Duration::hours(1);
    let refund = serde_json::json!({
        "op": "refund_order",
        "teller_id": manager,
        "request": {
            "order_id": order, "shift_id": shift, "amount": 100, "method": "cash",
            "reason": "quality_issue", "client_ref": cref, "issued_at": issued_at
        }
    });
    let r = replay(&app, &bearer, &refund).await;
    assert_eq!(r.status(), 201, "a manager's queued refund is admitted");
    let body: serde_json::Value = test::read_body_json(r).await;
    // `RefundIssued` flattens the refund, so its columns sit at the top level.
    assert_eq!(body["issued_by"], serde_json::json!(manager));
    assert_eq!(body["shift_id"], serde_json::json!(shift));
    assert_eq!(
        body["order_status"], "completed",
        "a partial refund leaves the status alone"
    );

    // The device re-flushes: the original comes back, no second payout.
    let r = replay(&app, &bearer, &refund).await;
    assert_eq!(r.status(), 200, "same client_ref → the original refund");
    let (count, total): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(amount), 0)::bigint FROM order_refunds WHERE order_id = $1",
    )
    .bind(order)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((count, total), (1, 100));

    // Revoke the grant per user and queue another refund. Phase 3 turned this
    // away with a 403. Phase 4 does not: a refund is money that ALREADY went
    // back across the counter while the shop was offline, and per the owner's
    // binding decision (PERMISSIONS_ARCHITECTURE §4.4.5) the act is accepted
    // and flagged for review rather than rejected. Rejecting it never
    // un-refunded the customer; it only lost the record and left the drawer
    // short. A void, where no money has moved, is still refused — see
    // `a_revoked_void_does_not_get_through_by_being_queued`.
    override_for(&pool, manager, "refunds", "create", false).await;
    let mut second = refund.clone();
    second["request"]["client_ref"] = serde_json::json!(Uuid::new_v4());
    let r = replay(&app, &bearer, &second).await;
    assert_eq!(
        r.status(),
        201,
        "the money already went back: accepted, not rejected"
    );
    let (cap, reason): (String, String) =
        sqlx::query_as("SELECT capability, reason FROM authz_replay_flags WHERE author_id = $1")
            .bind(manager)
            .fetch_one(&pool)
            .await
            .expect("the owner gets a flag for it");
    assert_eq!(cap, "refunds:create");
    // The revocation was written after the act's `issued_at` (an hour ago), so
    // the device WAS right when it acted and had simply not heard yet.
    assert_eq!(reason, "stale_snapshot");
}

/// A sale that ALREADY HAPPENED keeps the price the customer was charged.
///
/// This is the other half of the rule that stops a till pricing its own sales.
/// Live, the catalogue prices everything and a till's figure is ignored. But a
/// till that was offline when the shop changed a price took real money at the
/// number on its screen, and repricing that at replay time would make the
/// books disagree with the receipt in the customer's hand.
///
/// So the charged figure is recorded and the line is FLAGGED — which is what
/// makes it findable afterwards, and the reason the flag is worth having at
/// all now that it can only ever mean this.
#[sqlx::test]
async fn a_replayed_offline_sale_keeps_the_price_it_charged_and_is_flagged(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        grant(&pool, "teller", r, a).await;
    }
    let bearer = token(teller, org, UserRole::Teller);

    // The shop's menu says 500. This till sold it for 600 while it was offline,
    // because 600 was the price when it last synced.
    let resp = replay(
        &app,
        &bearer,
        &serde_json::json!({
            "op": "create_order",
            "teller_id": teller,
            "request": {
                "branch_id": branch,
                "shift_id": shift,
                "payment_method": "cash",
                "items": [{ "menu_item_id": item, "quantity": 1, "unit_price": 600 }],
                "total_amount": 684
            }
        }),
    )
    .await;
    assert!(resp.status().is_success(), "{:?}", resp.status());

    let (subtotal, flagged, expected): (i32, bool, Option<i32>) = sqlx::query_as(
        "SELECT subtotal, price_flagged, price_expected_total FROM orders \
          WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(subtotal, 600, "what the customer actually paid");
    assert!(flagged, "and it is findable afterwards");
    assert_eq!(expected, Some(570), "what the menu says today: 500 + 14%");
}

/// The sync app plus the live `/orders` route, for the two tests below that
/// have to prove the SAME reading on both paths.
macro_rules! app_with_orders {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::tickets::routes::configure)
                .configure(madar_rust::kitchen::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

/// A sale queued BEFORE the fraction migration still lands.
///
/// This blocked a production shop. Percentage discounts moved from `14` to
/// `0.14`, and the live path refuses the old spelling on purpose — a till
/// making a claim about a sale happening NOW in a language the server no
/// longer speaks must resync rather than sell. But the same refusal met every
/// order already sitting in a till's outbox, written before its app updated:
/// the money was in the drawer, the customer had gone, and the sale could not
/// reach the books at all.
///
/// A queued op is a RECORD, not a claim, and the convention it was written
/// under is knowable — under the new one a percentage cannot exceed 1.
#[sqlx::test]
async fn a_sale_queued_under_the_old_discount_model_still_lands(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        grant(&pool, "teller", r, a).await;
    }
    let bearer = token(teller, org, UserRole::Teller);

    // The old app's spelling: `14` meaning 14%. Ten pounds less 14% is 8.60,
    // and 14% tax on top makes 9.80 — which is the total it sent at the time.
    let resp = replay(
        &app,
        &bearer,
        &serde_json::json!({
            "op": "create_order",
            "teller_id": teller,
            "request": {
                "branch_id": branch,
                "shift_id": shift,
                "payment_method": "cash",
                "discount_type": "percentage",
                "discount_value": "14",
                "items": [{ "menu_item_id": item, "quantity": 1 }],
                "total_amount": 980
            }
        }),
    )
    .await;
    assert!(resp.status().is_success(), "{:?}", resp.status());

    let (dtype, dvalue, discount, total): (Option<String>, Decimal, i32, i32) = sqlx::query_as(
        "SELECT discount_type::text, discount_value, discount_amount, total_amount \
           FROM orders WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dtype.as_deref(), Some("percentage"));
    // Recorded in TODAY's convention, so a report reading this row beside a
    // new one is comparing the same kind of number.
    assert_eq!(dvalue, Decimal::new(14, 2), "0.14, not 14");
    assert_eq!(discount, 140, "14% of ten pounds");
    assert_eq!(total, 980);
}

/// A LIVE till on the old build can still sell.
///
/// This is the half that took a shop down. The server refused the old spelling
/// outright, so a branch that had not updated could not take a discounted sale
/// at all — not a sync problem, a till that would not ring. The reading is the
/// same on both paths now, and the total check below is what would catch it if
/// the till ever meant something else.
#[sqlx::test]
async fn a_live_till_on_the_old_build_can_still_sell(pool: PgPool) {
    let app = app_with_orders!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        grant(&pool, "teller", r, a).await;
    }
    let bearer = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "shift_id": shift,
                "payment_method": "cash",
                "discount_type": "percentage",
                "discount_value": "14",
                "items": [{ "menu_item_id": item, "quantity": 1 }],
                "total_amount": 980
            }))
            .to_request(),
    )
    .await;
    assert!(resp.status().is_success(), "{:?}", resp.status());
    let (dvalue, discount, total): (Decimal, i32, i32) = sqlx::query_as(
        "SELECT discount_value, discount_amount, total_amount FROM orders \
           WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        dvalue,
        Decimal::new(14, 2),
        "recorded in today's convention"
    );
    assert_eq!((discount, total), (140, 980));
}

/// A till that disagrees about the money is still refused.
///
/// The conversion is safe BECAUSE of this: reading `14` as 14% is only ever
/// accepted when the till's own total agrees with the server's arithmetic
/// afterwards. A payload that means something else fails here, loudly, rather
/// than giving a bill away.
#[sqlx::test]
async fn a_converted_discount_still_has_to_add_up(pool: PgPool) {
    let app = app_with_orders!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        grant(&pool, "teller", r, a).await;
    }
    let bearer = token(teller, org, UserRole::Teller);

    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(&serde_json::json!({
                "branch_id": branch,
                "shift_id": shift,
                "payment_method": "cash",
                "discount_type": "percentage",
                "discount_value": "14",
                "items": [{ "menu_item_id": item, "quantity": 1 }],
                // What a till would have sent if it really meant 1400% off.
                "total_amount": 0
            }))
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        409,
        "the totals disagree, so the sale is refused"
    );
}

/// Phase 5 (PERMISSIONS_ARCHITECTURE §4.2): a manager approved the act on the
/// till with their PIN. The queued op carries the approval; the server checks
/// the approver holds the act, lets it through, and keeps the record. An
/// approval by the author themself is worth nothing.
#[sqlx::test]
async fn a_manager_approval_carries_a_void_the_teller_does_not_hold(pool: PgPool) {
    let app = app!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let teller = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    let _shift = open_shift_row(&pool, branch, teller).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    grant(&pool, "branch_manager", "open_tickets", "delete").await;
    let bearer = token(teller, org, UserRole::Teller);

    let r = fire_by_replay(&app, &bearer, teller, branch, item).await;
    assert_eq!(r.status(), 201);
    let ticket_id = test::read_body_json::<OpenTicketView, _>(r).await.id;
    let cap = madar_rust::authz::Cap::from_legacy("open_tickets", "delete")
        .expect("a capability for the cell")
        .key();
    let void = |approver: Uuid| {
        serde_json::json!({
            "op": "void_open_ticket",
            "teller_id": teller,
            "ticket_id": ticket_id,
            "request": { "reason": "customer_request" },
            "approval": { "id": Uuid::new_v4(), "capability": cap, "approver_id": approver }
        })
    };

    let r = replay(&app, &bearer, &void(teller)).await;
    assert_eq!(r.status(), 403, "approving your own act approves nothing");

    let r = replay(&app, &bearer, &void(manager)).await;
    assert!(
        r.status().is_success(),
        "the manager's approval carries it: {}",
        r.status()
    );
    let (verified, approver): (bool, Uuid) = sqlx::query_as(
        "SELECT verified, approver_user_id FROM approvals WHERE subject_user_id = $1 AND op = 'VoidOpenTicket'",
    )
    .bind(teller)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(verified);
    assert_eq!(approver, manager);
}

/// Deferred feature 5: teller A started a cart, parked it, and teller B
/// resumed it after a teller switch and settled it. The replayed sale is B's
/// (drawer, reports) and records A as the person who started it.
///
/// Owner decision 2026-09-19: a resume is NOT an act that needs a grant
/// (capability 222 `orders.held.resume_others` is retired), so no such sale is
/// ever flagged for review — with or without the old manager approval on the
/// envelope, which a v0.7.9 till still sends and which is simply not looked
/// for. The attribution is what survives, and it is what the dashboard shows.
/// A `started_by` naming nobody of the org is dropped, never a refusal: the
/// sale happened.
#[sqlx::test]
async fn a_resumed_held_order_records_who_started_it_and_who_settled_it(pool: PgPool) {
    let app = app_with_orders!(pool);
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let item = seed_menu_item(&pool, org, 500).await;
    let ali = seed_user(&pool, org, "teller").await;
    let badr = seed_user(&pool, org, "teller").await;
    let manager = seed_user(&pool, org, "branch_manager").await;
    let other_org = seed_org(&pool).await;
    let stranger = seed_user(&pool, other_org, "teller").await;
    let shift = open_shift_row(&pool, branch, badr).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    for role in ["teller", "branch_manager"] {
        for (r, a) in [
            ("orders", "create"),
            ("orders", "read"),
            ("order_items", "create"),
            ("payments", "create"),
        ] {
            grant(&pool, role, r, a).await;
        }
    }
    let bearer = token(badr, org, UserRole::Teller);
    let sale = |started_by: Uuid, approval: Option<serde_json::Value>| {
        let mut v = serde_json::json!({
            "op": "create_order",
            "teller_id": badr,
            "request": {
                "branch_id": branch,
                "till_id": shift,
                "payment_method": "cash",
                "items": [{ "menu_item_id": item, "quantity": 1 }],
                "idempotency_key": Uuid::new_v4(),
                "started_by": started_by
            }
        });
        if let Some(a) = approval {
            v["approval"] = a;
        }
        v
    };

    // An OLD client (<= v0.7.9) still rings a manager's approval for the
    // resume and sends it. It must not regress: accepted, unflagged.
    let approval = serde_json::json!({ "id": Uuid::new_v4(), "capability": "orders.held.resume_others", "approver_id": manager });
    let r = replay(&app, &bearer, &sale(ali, Some(approval))).await;
    assert!(r.status().is_success(), "{}", r.status());
    let order: madar_rust::orders::handlers::OrderFull = test::read_body_json(r).await;
    assert_eq!(order.order.teller_id, badr, "settled by Badr: his drawer");
    assert_eq!(order.order.started_by, Some(ali), "started by Ali");
    assert!(order.order.started_by_name.is_some());

    // The order read back shows both people.
    let req = test::TestRequest::get()
        .uri(&format!("/orders/{}", order.order.id))
        .insert_header(("Authorization", format!("Bearer {bearer}")))
        .to_request();
    let r = test::call_service(&app, req).await;
    assert!(r.status().is_success(), "{}", r.status());
    let read: serde_json::Value = test::read_body_json(r).await;
    assert_eq!(read["teller_id"], serde_json::json!(badr));
    assert_eq!(read["started_by"], serde_json::json!(ali));
    assert!(read["started_by_name"].is_string());
    let (teller, by): (Uuid, Option<Uuid>) =
        sqlx::query_as("SELECT teller_id, started_by FROM orders WHERE id = $1")
            .bind(order.order.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((teller, by), (badr, Some(ali)));
    assert_eq!(
        flags_of(&pool, badr).await,
        0,
        "an old client's approved resume is clean"
    );

    // A name from another org is dropped, the sale still lands.
    let r = replay(&app, &bearer, &sale(stranger, None)).await;
    assert!(
        r.status().is_success(),
        "accept, never reject: {}",
        r.status()
    );
    let order: madar_rust::orders::handlers::OrderFull = test::read_body_json(r).await;
    assert_eq!(order.order.started_by, None);

    // Naming yourself records nothing extra.
    let r = replay(&app, &bearer, &sale(badr, None)).await;
    assert!(r.status().is_success());
    let order: madar_rust::orders::handlers::OrderFull = test::read_body_json(r).await;
    assert_eq!(order.order.started_by, None);

    // A NEW client (v0.7.10+): a plain teller settles Ali's order with no
    // approval at all. Accepted, attributed, and NOT flagged — the whole point
    // of the owner's decision. This is the line that used to file a flag.
    let r = replay(&app, &bearer, &sale(ali, None)).await;
    assert!(r.status().is_success(), "accepted: {}", r.status());
    let order: madar_rust::orders::handlers::OrderFull = test::read_body_json(r).await;
    assert_eq!(
        order.order.started_by,
        Some(ali),
        "the sale still names both"
    );
    assert_eq!(
        flags_of(&pool, badr).await,
        0,
        "a resume needs no grant, so it is never flagged"
    );

    // And a manager's held order resumed by that same plain teller: the sale
    // names the manager and is just as clean. Nobody's PIN was involved.
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    let r = replay(&app, &bearer, &sale(manager, None)).await;
    assert!(r.status().is_success(), "{}", r.status());
    let order: madar_rust::orders::handlers::OrderFull = test::read_body_json(r).await;
    assert_eq!(order.order.started_by, Some(manager));
    assert_eq!(flags_of(&pool, badr).await, 0, "still clean");

    // The other way round: the manager rings the teller's held order.
    let mbearer = token(manager, org, UserRole::BranchManager);
    let mshift = open_shift_row(&pool, branch, manager).await;
    let mut msale = sale(ali, None);
    msale["teller_id"] = serde_json::json!(manager);
    msale["request"]["till_id"] = serde_json::json!(mshift);
    let r = replay(&app, &mbearer, &msale).await;
    assert!(r.status().is_success(), "{}", r.status());
    assert_eq!(flags_of(&pool, manager).await, 0, "never flagged either way");
}

async fn flags_of(pool: &PgPool, author: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM authz_replay_flags WHERE author_id = $1")
        .bind(author)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Deferred feature 5: a till closed with held orders still parked records
/// how many were left open (and their total), through the live close route and
/// through a replayed close alike, and the Z report carries it. A close that
/// says nothing (an older till) stores nothing.
#[sqlx::test]
async fn a_close_records_the_held_orders_left_open(pool: PgPool) {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(BranchEventHub::new()))
            .configure(madar_rust::tills::routes::configure)
            .configure(madar_rust::sync::routes::configure),
    )
    .await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    for (r, a) in [("tills", "read"), ("tills", "update"), ("tills", "create")] {
        grant(&pool, "teller", r, a).await;
    }
    let bearer = token(teller, org, UserRole::Teller);
    let report = |till: Uuid| {
        test::TestRequest::get()
            .uri(&format!("/tills/{till}/report"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .to_request()
    };

    // Live close, two held orders worth 12.50 left.
    let live = open_shift_row(&pool, branch, teller).await;
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri(&format!("/tills/{live}/close"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(serde_json::json!({
                "closing_cash_declared": 0,
                "held_orders_left_open": 2,
                "held_orders_left_open_total": 1250
            }))
            .to_request(),
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    let rep: serde_json::Value =
        test::read_body_json(test::call_service(&app, report(live)).await).await;
    assert_eq!(rep["held_orders_left_open"], 2, "{rep}");
    assert_eq!(rep["held_orders_left_open_total"], 1250);
    assert_eq!(rep["till"]["held_orders_left_open"], 2);

    // Replayed close, one left.
    let queued = open_shift_row(&pool, branch, teller).await;
    let r = replay(
        &app,
        &bearer,
        &serde_json::json!({
            "op": "close_till", "teller_id": teller, "till_id": queued,
            "request": { "closing_cash_declared": 0, "held_orders_left_open": 1, "held_orders_left_open_total": 500 }
        }),
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    let rep: serde_json::Value =
        test::read_body_json(test::call_service(&app, report(queued)).await).await;
    assert_eq!(rep["held_orders_left_open"], 1, "{rep}");

    // An older till's close says nothing: nothing is stored.
    let old = open_shift_row(&pool, branch, teller).await;
    let r = replay(
        &app,
        &bearer,
        &serde_json::json!({ "op": "close_till", "teller_id": teller, "till_id": old,
            "request": { "closing_cash_declared": 0 } }),
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    let stored: Option<i32> =
        sqlx::query_scalar("SELECT held_orders_left_open FROM tills WHERE id = $1")
            .bind(old)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, None);
}

// ── Discounts (phase 6): per-person caps, accept-and-flag, manager approval ──

/// Cap a role's discount grant, as the dashboard's limits editor would.
async fn cap_role_grant(pool: &PgPool, user: Uuid, cap: i16, limits: serde_json::Value) {
    let n = sqlx::query(
        "UPDATE org_role_grants SET limits = $3
          WHERE capability_id = $2
            AND org_role_id IN (SELECT org_role_id FROM role_assignments WHERE user_id = $1)",
    )
    .bind(user)
    .bind(cap)
    .bind(limits)
    .execute(pool)
    .await
    .unwrap()
    .rows_affected();
    assert!(n > 0, "the person's role holds capability {cap}");
}

async fn discount_sale_fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid) {
    let org = seed_org(pool).await;
    let branch = seed_branch(pool, org).await;
    let item = seed_menu_item(pool, org, 2000).await;
    let teller = seed_user(pool, org, "teller").await;
    let manager = seed_user(pool, org, "branch_manager").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(manager)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    let shift = open_shift_row(pool, branch, teller).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '#000', 'cash', true, true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    for (r, a) in [
        ("orders", "create"),
        ("orders", "read"),
        ("order_items", "create"),
        ("payments", "create"),
    ] {
        grant(pool, "teller", r, a).await;
    }
    // A teller may take up to 10.00 off by hand.
    cap_role_grant(pool, teller, 205, serde_json::json!({ "max_amount": 1000 })).await;
    (org, branch, item, teller, manager, shift)
}

fn discounted_sale(
    teller: Uuid,
    branch: Uuid,
    shift: Uuid,
    item: Uuid,
    approval: Option<serde_json::Value>,
) -> serde_json::Value {
    // 20.00 less 15.00 by hand = 5.00, plus 14% tax = 5.70.
    let mut op = serde_json::json!({
        "op": "create_order",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "shift_id": shift,
            "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "discount_kind": "manual_amount",
            "discount_type": "fixed",
            "discount_value": 1500,
            "discount_amount": 1500,
            "discount_applied_by": teller,
            "items": [{ "menu_item_id": item, "quantity": 1 }],
            "total_amount": 570
        }
    });
    if let Some(a) = approval {
        op["request"]["discount_approval_id"] = a["id"].clone();
        op["approval"] = a;
    }
    op
}

/// The locked rule: a teller's over-cap discount without approval is accepted
/// (the money moved) and flagged; with a valid manager approval it is clean.
#[sqlx::test]
async fn a_tellers_over_cap_discount_is_flagged_without_approval_and_clean_with_one(pool: PgPool) {
    let app = app!(pool);
    let (org, branch, item, teller, manager, shift) = discount_sale_fixture(&pool).await;
    let bearer = token(teller, org, UserRole::Teller);

    // No approval: accepted, recorded, flagged against the sale.
    let r = replay(&app, &bearer, &discounted_sale(teller, branch, shift, item, None)).await;
    assert!(r.status().is_success(), "{:?}", r.status());
    let (order_id, kind, by, approval_id, amount): (Uuid, Option<String>, Option<Uuid>, Option<Uuid>, i32) =
        sqlx::query_as(
            "SELECT id, discount_kind, discount_applied_by, discount_approval_id, discount_amount
               FROM orders WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind.as_deref(), Some("manual_amount"));
    assert_eq!(by, Some(teller));
    assert_eq!(approval_id, None);
    assert_eq!(amount, 1500);
    let flags: Vec<(String, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT capability, reason, subject_id FROM authz_replay_flags WHERE author_id = $1",
    )
    .bind(teller)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        flags,
        vec![(
            "orders.discount.manual_amount".to_string(),
            "unauthorized_offline".to_string(),
            Some(order_id)
        )]
    );

    // An approval by the teller themself is worth nothing: still flagged.
    let own = serde_json::json!({
        "id": Uuid::new_v4(), "capability": "orders.discount.manual_amount",
        "approver_id": teller, "amount_minor": 1500
    });
    let r = replay(&app, &bearer, &discounted_sale(teller, branch, shift, item, Some(own))).await;
    assert!(r.status().is_success());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM authz_replay_flags WHERE author_id = $1")
        .bind(teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2, "a self-approval does not clear the flag");

    // A manager who holds the act approved it: clean, and the approval is kept.
    let approval_id = Uuid::new_v4();
    let good = serde_json::json!({
        "id": approval_id, "capability": "orders.discount.manual_amount",
        "approver_id": manager, "amount_minor": 1500
    });
    let r = replay(&app, &bearer, &discounted_sale(teller, branch, shift, item, Some(good))).await;
    assert!(r.status().is_success(), "{:?}", r.status());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM authz_replay_flags WHERE author_id = $1")
        .bind(teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 2, "the approved discount adds no flag");
    let stored: Option<Uuid> = sqlx::query_scalar(
        "SELECT discount_approval_id FROM orders WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, Some(approval_id));
    let verified: bool = sqlx::query_scalar("SELECT verified FROM approvals WHERE id = $1")
        .bind(approval_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(verified);
}

/// Within the cap nothing is flagged; a percentage cap is in basis points.
#[sqlx::test]
async fn a_discount_within_the_cap_is_clean_and_a_percent_cap_counts_basis_points(pool: PgPool) {
    let app = app!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    cap_role_grant(&pool, teller, 206, serde_json::json!({ "max_percent": 1000 })).await;
    let bearer = token(teller, org, UserRole::Teller);

    // 10.00 off by hand: at the cap.
    let mut op = discounted_sale(teller, branch, shift, item, None);
    op["request"]["discount_value"] = 1000.into();
    op["request"]["discount_amount"] = 1000.into();
    op["request"]["total_amount"] = 1140.into();
    let r = replay(&app, &bearer, &op).await;
    assert!(r.status().is_success(), "{:?}", r.status());

    // 10% by hand: at the cap. 20.00 less 2.00 = 18.00 + 14% = 20.52.
    let pct = |bps: i64, total: i64, amount: i64| {
        serde_json::json!({
            "op": "create_order", "teller_id": teller,
            "request": {
                "branch_id": branch, "shift_id": shift, "payment_method": "cash",
                "idempotency_key": Uuid::new_v4(),
                "discount_kind": "manual_percent", "discount_type": "percentage",
                "discount_value": bps as f64 / 10000.0, "discount_percent_bps": bps,
                "discount_amount": amount,
                "items": [{ "menu_item_id": item, "quantity": 1 }],
                "total_amount": total
            }
        })
    };
    let r = replay(&app, &bearer, &pct(1000, 2052, 200)).await;
    assert!(r.status().is_success(), "{:?}", r.status());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM authz_replay_flags WHERE author_id = $1")
        .bind(teller)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "within both caps");

    // 12.5%: over the percent cap. 20.00 less 2.50 = 17.50 + 14% = 19.95.
    let r = replay(&app, &bearer, &pct(1250, 1995, 250)).await;
    assert!(r.status().is_success(), "{:?}", r.status());
    let caps: Vec<String> =
        sqlx::query_scalar("SELECT capability FROM authz_replay_flags WHERE author_id = $1")
            .bind(teller)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(caps, vec!["orders.discount.manual_percent".to_string()]);
    let bps: Option<i32> = sqlx::query_scalar(
        "SELECT discount_percent_bps FROM orders WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bps, Some(1250));
}

/// The live route has no manager on hand: over the cap is a 403, and a person
/// without the discount capability at all is refused before anything is written.
#[sqlx::test]
async fn the_live_order_route_refuses_a_discount_over_the_cap_or_without_the_capability(
    pool: PgPool,
) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    let bearer = token(teller, org, UserRole::Teller);
    let post = |body: serde_json::Value| {
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(body)
            .to_request()
    };
    let body = |amount: i64| {
        serde_json::json!({
            "branch_id": branch, "shift_id": shift, "payment_method": "cash",
            "discount_kind": "manual_amount", "discount_type": "fixed",
            "discount_value": amount, "discount_amount": amount,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })
    };
    let r = test::call_service(&app, post(body(1500))).await;
    assert_eq!(r.status(), 403, "over the teller's 10.00 cap");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE branch_id = $1")
        .bind(branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);

    let r = test::call_service(&app, post(body(500))).await;
    assert!(r.status().is_success(), "within the cap: {:?}", r.status());
    let v: serde_json::Value = test::read_body_json(r).await;
    assert_eq!(v["discount_kind"], "manual_amount");
    assert_eq!(v["discount_applied_by"], serde_json::json!(teller));

    // Without the capability at all.
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, reason) \
         VALUES ($1, $2, 205, 'deny', 'test')",
    )
    .bind(org)
    .bind(teller)
    .execute(&pool)
    .await
    .unwrap();
    let r = test::call_service(&app, post(body(100))).await;
    assert_eq!(r.status(), 403, "no orders.discount.manual_amount");
}

/// The live route now takes a manager's one-time PIN unlock (owner,
/// 2026-09-17): the SAME rule as replay, checked by the SAME
/// `verify_approval`. A valid approval lets an over-cap discount through and
/// records who approved it; an approver missing the capability, or approving
/// their own act, is still refused; the live and replay verdicts agree.
#[sqlx::test]
async fn a_live_discount_over_the_cap_with_a_managers_pin_is_allowed_and_recorded(pool: PgPool) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, manager, shift) = discount_sale_fixture(&pool).await;
    for (r, a) in [("orders", "read")] {
        grant(&pool, "branch_manager", r, a).await;
    }
    cap_role_grant(&pool, manager, 205, serde_json::json!({})).await;
    let bearer = token(teller, org, UserRole::Teller);
    let post = |body: serde_json::Value| {
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(body)
            .to_request()
    };
    let mut over_cap = serde_json::json!({
        "branch_id": branch, "shift_id": shift, "payment_method": "cash",
        "discount_kind": "manual_amount", "discount_type": "fixed",
        "discount_value": 1500, "discount_amount": 1500,
        "items": [{ "menu_item_id": item, "quantity": 1 }]
    });

    // A stranger's PIN doesn't hold the discount capability: still refused.
    let stranger = seed_user(&pool, org, "teller").await;
    over_cap["live_approval"] = serde_json::json!({
        "id": Uuid::new_v4(), "capability": "orders.discount.manual_amount",
        "approver_id": stranger, "amount_minor": 1500,
    });
    let r = test::call_service(&app, post(over_cap.clone())).await;
    assert_eq!(r.status(), 403, "the approver doesn't hold it either");

    // The teller cannot approve their own over-cap discount.
    over_cap["live_approval"]["approver_id"] = serde_json::json!(teller);
    let r = test::call_service(&app, post(over_cap.clone())).await;
    assert_eq!(r.status(), 403, "self-approval is refused");

    // A manager's PIN unlocks it.
    over_cap["live_approval"]["approver_id"] = serde_json::json!(manager);
    let approval_id = over_cap["live_approval"]["id"].clone();
    let r = test::call_service(&app, post(over_cap)).await;
    assert!(r.status().is_success(), "{:?}", r.status());
    let v: serde_json::Value = test::read_body_json(r).await;
    assert_eq!(v["discount_applied_by"], serde_json::json!(teller));
    assert_eq!(v["discount_approval_id"], approval_id);
    let approver: Uuid =
        sqlx::query_scalar("SELECT approver_user_id FROM approvals WHERE id = $1")
            .bind(uuid::Uuid::parse_str(approval_id.as_str().unwrap()).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(approver, manager);
}

/// An allow override's limits replace the role's: the cap a till reads for a
/// hand-typed discount comes from the person's override.
#[sqlx::test]
async fn an_allow_override_caps_a_discount_for_one_person(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, limits, reason)
         VALUES ($1, $2, 205, 'allow', '{\"max_amount\": 500}'::jsonb, 'test')",
    )
    .bind(org)
    .bind(teller)
    .execute(&pool)
    .await
    .unwrap();
    let eff = madar_rust::authz::require::effective(&pool, teller, Some(branch)).await.unwrap();
    assert_eq!(
        eff.limits_of(madar_rust::authz::Cap::OrdersDiscountManualAmount).max_amount,
        Some(500)
    );
}

// ── Discounts on a TABLE'S BILL (stream 9) ────────────────────────────────
//
// A bill is a sale. Until now the settle path applied a discount with no
// capability check and no cap — the one way round the counter-sale gate.

/// A fired ticket, ready to settle, on a fixture that caps the teller's manual
/// discount at 10.00 (`discount_sale_fixture`).
async fn fired_ticket(
    app: &impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    >,
    bearer: &str,
    teller: Uuid,
    branch: Uuid,
    item: Uuid,
) -> Uuid {
    let r = fire_by_replay(app, bearer, teller, branch, item).await;
    assert_eq!(r.status(), 201, "the ticket fires");
    test::read_body_json::<OpenTicketView, _>(r).await.id
}

fn settle_op(
    teller: Uuid,
    ticket: Uuid,
    shift: Uuid,
    discount: serde_json::Value,
    approval: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut request = serde_json::json!({
        "till_id": shift,
        "payment_method": "cash",
    });
    for (k, v) in discount.as_object().unwrap() {
        request[k] = v.clone();
    }
    let mut op = serde_json::json!({
        "op": "settle_open_ticket",
        "teller_id": teller,
        "ticket_id": ticket,
        "request": request,
    });
    if let Some(a) = approval {
        op["request"]["discount_approval_id"] = a["id"].clone();
        op["approval"] = a;
    }
    op
}

async fn bill_flags(pool: &PgPool, teller: Uuid) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT capability, reason FROM authz_replay_flags
          WHERE author_id = $1 AND op = 'SettleOpenTicket' ORDER BY capability",
    )
    .bind(teller)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The locked rule, now for a bill: a queued settle whose discount is over the
/// cashier's cap is ACCEPTED (the party paid and left) and flagged. A manager's
/// PIN approval carried by the op clears it and is recorded on the order.
#[sqlx::test]
async fn a_table_bills_over_cap_discount_is_flagged_without_approval_and_clean_with_one(
    pool: PgPool,
) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, manager, shift) = discount_sale_fixture(&pool).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    let bearer = token(teller, org, UserRole::Teller);

    // 20.00 off by hand on a 20.00 bill: double the teller's 10.00 cap.
    let over = serde_json::json!({
        "discount_kind": "manual_amount",
        "discount_type": "fixed",
        "discount_value": 2000,
        "discount_amount": 2000
    });

    let ticket = fired_ticket(&app, &bearer, teller, branch, item).await;
    let r = replay(&app, &bearer, &settle_op(teller, ticket, shift, over.clone(), None)).await;
    assert!(r.status().is_success(), "the bill lands anyway: {}", r.status());
    assert_eq!(
        bill_flags(&pool, teller).await,
        vec![(
            "orders.discount.manual_amount".to_string(),
            "unauthorized_offline".to_string()
        )],
        "accepted and flagged, never refused"
    );
    let (kind, by, appr, amount): (Option<String>, Option<Uuid>, Option<Uuid>, i32) =
        sqlx::query_as(
            "SELECT discount_kind, discount_applied_by, discount_approval_id, discount_amount
               FROM orders WHERE open_ticket_id = $1",
        )
        .bind(ticket)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kind.as_deref(), Some("manual_amount"), "attributed like a counter sale");
    assert_eq!(by, Some(teller));
    assert_eq!(appr, None);
    assert_eq!(amount, 2000, "the money as the drawer took it");

    // A second bill, this one with a manager's approval: clean.
    let approval_id = Uuid::new_v4();
    let good = serde_json::json!({
        "id": approval_id, "capability": "orders.discount.manual_amount",
        "approver_id": manager, "amount_minor": 2000
    });
    let ticket2 = fired_ticket(&app, &bearer, teller, branch, item).await;
    let r = replay(&app, &bearer, &settle_op(teller, ticket2, shift, over, Some(good))).await;
    assert!(r.status().is_success(), "{}", r.status());
    assert_eq!(
        bill_flags(&pool, teller).await.len(),
        1,
        "the approved bill adds no flag"
    );
    let stored: Option<Uuid> =
        sqlx::query_scalar("SELECT discount_approval_id FROM orders WHERE open_ticket_id = $1")
            .bind(ticket2)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(approval_id), "the approval rides onto the order");
    let verified: bool = sqlx::query_scalar("SELECT verified FROM approvals WHERE id = $1")
        .bind(approval_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(verified);
}

/// Within the cap, a bill is clean — and a discount the CASHIER never mentions,
/// inherited in silence from the waiter's ticket, is judged all the same. That
/// silence was the hole: the figure charged was the waiter's and nobody asked.
#[sqlx::test]
async fn a_bill_within_the_cap_is_clean_and_an_inherited_waiter_discount_is_still_judged(
    pool: PgPool,
) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    let bearer = token(teller, org, UserRole::Teller);

    // 5.00 off, half the cap.
    let under = serde_json::json!({
        "discount_kind": "manual_amount",
        "discount_type": "fixed",
        "discount_value": 500,
        "discount_amount": 500
    });
    let ticket = fired_ticket(&app, &bearer, teller, branch, item).await;
    let r = replay(&app, &bearer, &settle_op(teller, ticket, shift, under, None)).await;
    assert!(r.status().is_success());
    assert!(bill_flags(&pool, teller).await.is_empty(), "within the cap");

    // Now a ticket the WAITER discounted by 20.00, settled in silence.
    let ticket2 = fired_ticket(&app, &bearer, teller, branch, item).await;
    sqlx::query(
        "UPDATE open_tickets SET discount_type = 'fixed', discount_value = 2000 WHERE id = $1",
    )
    .bind(ticket2)
    .execute(&pool)
    .await
    .unwrap();
    let r = replay(
        &app,
        &bearer,
        &settle_op(teller, ticket2, shift, serde_json::json!({}), None),
    )
    .await;
    assert!(r.status().is_success(), "the bill still lands");
    assert_eq!(
        bill_flags(&pool, teller).await,
        vec![(
            "orders.discount.manual_amount".to_string(),
            "unauthorized_offline".to_string()
        )],
        "a discount inherited in silence is the same act, and over the cap"
    );
}

/// LIVE, there is no manager on hand: a bill discount over the cashier's cap is
/// refused outright (403), exactly as `POST /orders` refuses a counter sale's.
#[sqlx::test]
async fn a_live_settle_refuses_a_bill_discount_over_the_cashiers_cap(pool: PgPool) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    let bearer = token(teller, org, UserRole::Teller);
    let ticket = fired_ticket(&app, &bearer, teller, branch, item).await;

    let settle = |value: i64| {
        test::TestRequest::post()
            .uri(&format!("/open-tickets/{ticket}/settle"))
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(serde_json::json!({
                "till_id": shift,
                "payment_method": "cash",
                "discount_kind": "manual_amount",
                "discount_type": "fixed",
                "discount_value": value,
                "discount_amount": value
            }))
            .to_request()
    };

    let r = test::call_service(&app, settle(2000)).await;
    assert_eq!(r.status(), 403, "over the cap, live: refused, not flagged");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE open_ticket_id = $1")
        .bind(ticket)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "nothing was booked");

    // Within the cap the same bill settles, and names who discounted it.
    let r = test::call_service(&app, settle(1000)).await;
    assert!(r.status().is_success(), "{}", r.status());
    let (kind, by): (Option<String>, Option<Uuid>) = sqlx::query_as(
        "SELECT discount_kind, discount_applied_by FROM orders WHERE open_ticket_id = $1",
    )
    .bind(ticket)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(kind.as_deref(), Some("manual_amount"));
    assert_eq!(by, Some(teller), "the cashier holding the token");
}

/// The live settle route takes the same manager-PIN unlock as `POST /orders`
/// (owner, 2026-09-17): over the cashier's cap, a valid live approval lets the
/// bill settle and records who approved it.
#[sqlx::test]
async fn a_live_settle_takes_a_managers_pin_over_the_cashiers_cap(pool: PgPool) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, manager, shift) = discount_sale_fixture(&pool).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    cap_role_grant(&pool, manager, 205, serde_json::json!({})).await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    let bearer = token(teller, org, UserRole::Teller);
    let ticket = fired_ticket(&app, &bearer, teller, branch, item).await;
    let approval_id = Uuid::new_v4();
    let settle = test::TestRequest::post()
        .uri(&format!("/open-tickets/{ticket}/settle"))
        .insert_header(("Authorization", format!("Bearer {bearer}")))
        .set_json(serde_json::json!({
            "till_id": shift,
            "payment_method": "cash",
            "discount_kind": "manual_amount",
            "discount_type": "fixed",
            "discount_value": 2000,
            "discount_amount": 2000,
            "live_approval": {
                "id": approval_id, "capability": "orders.discount.manual_amount",
                "approver_id": manager, "amount_minor": 2000,
            }
        }))
        .to_request();
    let r = test::call_service(&app, settle).await;
    assert!(r.status().is_success(), "{}", r.status());
    let (by, approval): (Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT discount_applied_by, discount_approval_id FROM orders WHERE open_ticket_id = $1",
    )
    .bind(ticket)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(by, Some(teller));
    assert_eq!(approval, Some(approval_id));
}

/// A preset switched OFF while the till was offline. The sale already happened:
/// it lands with the amount AS RUNG (never recomputed from the dead rule) and is
/// flagged for the owner. Live, the same preset is still a clean error.
#[sqlx::test]
async fn a_replayed_sale_whose_preset_was_switched_off_lands_with_the_amount_as_rung(
    pool: PgPool,
) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1, $2)")
        .bind(teller)
        .bind(branch)
        .execute(&pool)
        .await
        .unwrap();
    // "Staff 25%", generous enough to need the preset grant but not a cap.
    let preset = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO discounts (id, org_id, name, type, value, is_active) \
         VALUES ($1, $2, 'Staff', 'percentage', 0.25, false)",
    )
    .bind(preset)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let bearer = token(teller, org, UserRole::Teller);

    // LIVE: a cashier picking a dead rule gets a clean error. Nothing happened
    // yet, and they can pick another.
    let r = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/orders")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(serde_json::json!({
                "branch_id": branch, "till_id": shift, "payment_method": "cash",
                "discount_id": preset, "discount_amount": 500,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }))
            .to_request(),
    )
    .await;
    assert_eq!(r.status(), 400, "live, a dead preset is a clean error");

    // REPLAY: the money moved. 5.00 was what the till took off under the rule
    // as it stood; that figure stands, and the owner is told.
    let key = Uuid::new_v4();
    let r = replay(
        &app,
        &bearer,
        &serde_json::json!({
            "op": "create_order",
            "teller_id": teller,
            "request": {
                "branch_id": branch, "till_id": shift, "payment_method": "cash",
                "idempotency_key": key,
                "discount_id": preset, "discount_kind": "preset",
                "discount_amount": 500,
                "items": [{ "menu_item_id": item, "quantity": 1 }]
            }
        }),
    )
    .await;
    assert!(r.status().is_success(), "the sale lands: {}", r.status());
    let amount: i32 =
        sqlx::query_scalar("SELECT discount_amount FROM orders WHERE idempotency_key = $1")
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(amount, 500, "as rung — NOT 25% of whatever the basket is now");
    let flags: Vec<(String, String)> = sqlx::query_as(
        "SELECT capability, reason FROM authz_replay_flags WHERE author_id = $1",
    )
    .bind(teller)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        flags.contains(&(
            "orders.discount.preset:inactive".to_string(),
            "unauthorized_offline".to_string()
        )),
        "the owner is told which rule is dead: {flags:?}"
    );
}

/// A bill paid in two goes — the party's own split, one settle per financial
/// transaction — carries its OWN discount ask and its own approval each time.
/// Nobody gets a second discount free on the back of the first one's approval.
#[sqlx::test]
async fn each_settle_of_a_split_bill_answers_for_its_own_discount(pool: PgPool) {
    let app = app_with_orders!(pool);
    let (org, branch, item, teller, manager, shift) = discount_sale_fixture(&pool).await;
    grant(&pool, "teller", "open_tickets", "create").await;
    grant(&pool, "teller", "open_tickets", "update").await;
    let bearer = token(teller, org, UserRole::Teller);

    let over = serde_json::json!({
        "discount_kind": "manual_amount",
        "discount_type": "fixed",
        "discount_value": 2000,
        "discount_amount": 2000
    });

    // Half the party: approved by the manager. Clean.
    let first = fired_ticket(&app, &bearer, teller, branch, item).await;
    let good = serde_json::json!({
        "id": Uuid::new_v4(), "capability": "orders.discount.manual_amount",
        "approver_id": manager, "amount_minor": 2000
    });
    let r = replay(
        &app,
        &bearer,
        &settle_op(teller, first, shift, over.clone(), Some(good)),
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    assert!(bill_flags(&pool, teller).await.is_empty());

    // The other half, same discount, no approval of its own: flagged. The
    // approval on the first settle does not stretch over the second.
    let second = fired_ticket(&app, &bearer, teller, branch, item).await;
    let r = replay(&app, &bearer, &settle_op(teller, second, shift, over, None)).await;
    assert!(r.status().is_success(), "the money still lands");
    assert_eq!(
        bill_flags(&pool, teller).await,
        vec![(
            "orders.discount.manual_amount".to_string(),
            "unauthorized_offline".to_string()
        )],
        "the discount is per settle, not per party"
    );
}

/// THE TWO HALVES AGREE. A queued void and a queued refund are now judged by
/// the same `authz::acts` limits the live routes ask — and they part company
/// only where the locked rule says they must (§4.4.5): a void moved no money,
/// so an over-limit one is refused here as it is live; a refund did, so it
/// lands and is flagged for the owner.
#[sqlx::test]
async fn the_void_and_refund_limits_hold_on_replay_too(pool: PgPool) {
    let app = app!(pool);
    madar_rust::permissions::seeder::seed_role_permissions(&pool)
        .await
        .unwrap();
    let org = seed_org(&pool).await;
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) \
         VALUES ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true)",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let branch = seed_branch(&pool, org).await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    // The provisioned teller default: own sale, ten minutes; refunds capped at
    // nothing, so every refund asks a manager.
    for (cap, limits) in [
        (64, serde_json::json!({"own": true, "max_age_minutes": 10})),
        (69, serde_json::json!({"max_amount": 0})),
    ] {
        sqlx::query(
            "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, limits, reason) \
             VALUES ($1, $2, $3, 'allow', $4, 'test')",
        )
        .bind(org)
        .bind(teller)
        .bind(cap)
        .bind(limits)
        .execute(&pool)
        .await
        .unwrap();
    }
    let seed_order = |n: i32, mins: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO orders (branch_id, teller_id, till_id, idempotency_key, subtotal, \
                                     tax_amount, total_amount, status, order_number, \
                                     payment_method, order_ref, created_at) \
                 VALUES ($1, $2, $3, gen_random_uuid(), 300, 0, 300, 'completed', $4, 'cash', \
                         gen_random_uuid()::text, now() - make_interval(mins => $5)) RETURNING id",
            )
            .bind(branch)
            .bind(teller)
            .bind(shift)
            .bind(n)
            .bind(mins)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let bearer = token(teller, org, UserRole::Teller);
    let void = |order: Uuid, at: chrono::DateTime<chrono::Utc>| {
        serde_json::json!({
            "op": "void_order", "teller_id": teller, "order_id": order,
            "occurred_at": at,
            "request": { "reason": "mistake", "voided_at": at }
        })
    };

    // Judged at the moment it was rung, not at the moment the queue drained:
    // a two-minute-old sale voided offline still lands an hour later.
    let fresh = seed_order(1, 2).await;
    let r = replay(&app, &bearer, &void(fresh, chrono::Utc::now())).await;
    assert!(
        r.status().is_success(),
        "own sale inside the window: {}",
        r.status()
    );

    let stale = seed_order(2, 40).await;
    let r = replay(&app, &bearer, &void(stale, chrono::Utc::now())).await;
    assert_eq!(r.status(), 403, "own sale, 40 minutes old, no approval");

    // A refund over the cap is NOT refused — the customer already has the
    // money — it lands and the owner is told.
    let paid = seed_order(3, 5).await;
    let refund = serde_json::json!({
        "op": "refund_order", "teller_id": teller,
        "request": { "order_id": paid, "shift_id": shift, "amount": 100, "method": "cash",
                     "reason": "quality_issue", "client_ref": Uuid::new_v4() }
    });
    let r = replay(&app, &bearer, &refund).await;
    assert_eq!(r.status(), 201, "a queued refund over the cap still lands");
    let flagged: Vec<String> = sqlx::query_scalar(
        "SELECT capability FROM authz_replay_flags WHERE org_id = $1 AND op = 'RefundOrder'",
    )
    .bind(org)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(flagged, vec!["refunds.create".to_string()]);
}

// ────────────────────────────────────────────────────────────────────────
// A negative order is refused AT REPLAY (owner, 2026-09-18)
//
// This is the one validation that refuses a money op at replay, and it does
// not contradict accept-and-flag (§4.4.5). That rule answers "MAY this actor
// do this", and says a sale whose money already moved is recorded even when
// the grant was missing. This answers "IS this a sale at all". A negative
// total is not a sale that happened — it is corrupt input, from a bad price
// or a stale build — and there is no honest row to write for it. It is
// refused for exactly the reason `quantity <= 0` already is.
//
// A refused op dead-letters on the till and surfaces in the stuck list, so an
// old client (<= v0.7.8) that queued one does not retry for ever, and the
// owner is shown it rather than finding a negative sale in the books.
// ────────────────────────────────────────────────────────────────────────

/// A replayed sale whose stated subtotal is below zero. This is the payload
/// shape that used to PANIC the handler (`clamp(0, subtotal)` with
/// `min > max`) rather than be refused.
#[sqlx::test]
async fn a_replayed_order_with_a_negative_subtotal_is_refused_not_booked(pool: PgPool) {
    let app = app!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    let bearer = token(teller, org, UserRole::Teller);

    let op = serde_json::json!({
        "op": "create_order",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "shift_id": shift,
            "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": [{ "menu_item_id": item, "quantity": 1 }],
            "subtotal": -2000,
            "total_amount": -2280
        }
    });
    let r = replay(&app, &bearer, &op).await;
    assert_eq!(r.status(), 400, "a negative sale is corrupt input, not a sale");

    let booked: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE branch_id = $1")
        .bind(branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(booked, 0, "nothing negative may reach the books");
}

/// A replayed line priced below zero. Replay is the ONE path that takes the
/// till's line prices verbatim (`ClientPrices::AsCharged`), because the sale
/// already happened at the price on the screen — which is exactly why it is
/// also the one path a negative line can arrive on.
#[sqlx::test]
async fn a_replayed_line_priced_below_zero_is_refused(pool: PgPool) {
    let app = app!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    let bearer = token(teller, org, UserRole::Teller);

    let op = serde_json::json!({
        "op": "create_order",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "shift_id": shift,
            "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": [{ "menu_item_id": item, "quantity": 1, "unit_price": -500 }],
            "total_amount": 0
        }
    });
    assert_eq!(replay(&app, &bearer, &op).await.status(), 400);

    let booked: i64 = sqlx::query_scalar("SELECT count(*) FROM orders WHERE branch_id = $1")
        .bind(branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(booked, 0);
}

/// The other half at replay: an over-large discount is still CAPPED and the
/// sale still lands. Refusing here would lose a sale that really happened —
/// the distinction the whole rule turns on.
#[sqlx::test]
async fn a_replayed_discount_bigger_than_the_bill_is_capped_and_still_booked(pool: PgPool) {
    let app = app!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    let bearer = token(teller, org, UserRole::Teller);

    // The item is 20.00; the till rang 999.99 off it.
    let op = serde_json::json!({
        "op": "create_order",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "shift_id": shift,
            "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "discount_kind": "manual_amount",
            "discount_type": "fixed",
            "discount_value": 99999,
            "discount_amount": 99999,
            "discount_applied_by": teller,
            "items": [{ "menu_item_id": item, "quantity": 1 }],
            "total_amount": 0
        }
    });
    let r = replay(&app, &bearer, &op).await;
    assert!(r.status().is_success(), "{:?}", r.status());

    let (subtotal, discount, total): (i32, i32, i32) = sqlx::query_as(
        "SELECT subtotal, discount_amount, total_amount FROM orders \
         WHERE branch_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(discount, subtotal, "capped at the bill, never past it");
    assert_eq!(total, 0, "zero, never below");
}

/// A modifier priced below nothing, hiding inside a line that is comfortably
/// positive. The line guard cannot see this one: -5.00 of modifier on a 20.00
/// coffee leaves the LINE at 15.00, and the negative `order_item_addons` row
/// underneath it is what the add-on revenue reports sum. The sale looks right
/// and the modifier's revenue goes backwards.
#[sqlx::test]
async fn a_replayed_modifier_priced_below_nothing_is_refused_even_inside_a_positive_line(
    pool: PgPool,
) {
    let app = app!(pool);
    let (org, branch, item, teller, _manager, shift) = discount_sale_fixture(&pool).await;
    let bearer = token(teller, org, UserRole::Teller);

    let addon = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO addon_items (id, org_id, name, default_price, type, is_active) \
         VALUES ($1, $2, 'Extra shot', 500, 'generic', true)",
    )
    .bind(addon)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();

    let op = serde_json::json!({
        "op": "create_order",
        "teller_id": teller,
        "request": {
            "branch_id": branch,
            "shift_id": shift,
            "payment_method": "cash",
            "idempotency_key": Uuid::new_v4(),
            "items": [{
                "menu_item_id": item,
                "quantity": 1,
                // The line still comes to 20.00 - 5.00 = 15.00: positive.
                "addons": [{ "addon_item_id": addon, "quantity": 1, "unit_price": -500 }]
            }]
        }
    });
    assert_eq!(replay(&app, &bearer, &op).await.status(), 400);

    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM order_item_addons a \
         JOIN order_items i ON i.id = a.order_item_id \
         JOIN orders o ON o.id = i.order_id WHERE o.branch_id = $1",
    )
    .bind(branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 0, "no negative add-on row may reach the revenue reports");
}
