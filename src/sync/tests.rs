//! `/sync/replay` authorization: attribution is the role's only job, and the
//! permission TABLE answers what a queued op may do — the same table, through
//! the same resolver, as the live route. Each test here is one way the old
//! hard-coded role → op match used to disagree with that table.

use actix_web::{App, test, web};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::{JwtSecret, create_token};
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
async fn open_shift_row(pool: &PgPool, branch: Uuid, teller: Uuid) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO shifts (branch_id, teller_id, status, opening_cash) \
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
                .configure(crate::tickets::routes::configure)
                .configure(crate::kitchen::routes::configure)
                .configure(crate::sync::routes::configure),
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
    crate::permissions::seeder::seed_role_permissions(&pool)
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

/// Attribution is the one thing the role still decides: whose name may go on
/// a replayed write. An org admin holds every grant there is and is still
/// refused — they never PIN in at a till, so no queued op can be theirs — and
/// so is a till user of another org, whatever they hold there.
#[sqlx::test]
async fn only_a_till_user_of_this_org_can_be_the_author(pool: PgPool) {
    let app = app!(pool);
    crate::permissions::seeder::seed_role_permissions(&pool)
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
    assert_eq!(r.status(), 403, "an admin is not a till user");
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
    crate::permissions::seeder::seed_role_permissions(&pool)
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
        "INSERT INTO orders (branch_id, teller_id, shift_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
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

    // The table is the authority offline too: revoke the grant per user and
    // the same op is turned away.
    override_for(&pool, manager, "refunds", "create", false).await;
    let mut second = refund.clone();
    second["request"]["client_ref"] = serde_json::json!(Uuid::new_v4());
    let r = replay(&app, &bearer, &second).await;
    assert_eq!(
        r.status(),
        403,
        "a per-user revocation of refunds:create holds offline"
    );
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
                .configure(crate::orders::routes::configure)
                .configure(crate::tickets::routes::configure)
                .configure(crate::kitchen::routes::configure)
                .configure(crate::sync::routes::configure),
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
