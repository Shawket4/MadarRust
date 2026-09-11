use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::jwt::JwtSecret;
use crate::models::UserRole;
use crate::refunds::handlers::{RefundIssued, shift_cash_refunds};
use crate::refunds::routes;

fn get_secret() -> JwtSecret {
    JwtSecret("secret".to_string())
}

fn teller_token(user_id: Uuid, org_id: Uuid, branch_id: Uuid) -> String {
    crate::auth::jwt::create_token(
        &get_secret(),
        user_id,
        Some(org_id),
        UserRole::Teller,
        Some(branch_id),
        24,
    )
    .unwrap()
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
    let org_id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Refund Org', $2)")
        .bind(org_id)
        .bind(format!("refund-org-{org_id}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES
        ($1, 'cash', '{}', 'emerald', 'payments_outlined', true, true),
        ($1, 'card', '{}', 'blue', 'credit_card_rounded', false, true)",
    )
    .bind(org_id)
    .execute(pool)
    .await
    .unwrap();
    org_id
}

async fn seed_branch(pool: &PgPool, org_id: Uuid) -> Uuid {
    let branch_id = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, $3)")
        .bind(branch_id)
        .bind(org_id)
        // Branch names are unique per org; a test that seats a second branch
        // needs a second name.
        .bind(format!("Branch {}", &branch_id.to_string()[..8]))
        .execute(pool)
        .await
        .unwrap();
    branch_id
}

async fn seed_user(pool: &PgPool, org_id: Uuid, role: &str) -> Uuid {
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1, $2, $5, $3, 'hash', $4::user_role)",
    )
    .bind(user_id)
    .bind(org_id)
    .bind(format!("user-{user_id}@test.com"))
    .bind(role)
    // Teller names are unique per org (`idx_users_teller_unique_name_per_org`).
    .bind(format!("Till {}", &user_id.to_string()[..8]))
    .execute(pool)
    .await
    .unwrap();
    user_id
}

async fn grant(pool: &PgPool, role: &str, resource: &str, action: &str) {
    sqlx::query(
        "INSERT INTO role_permissions (role, resource, action, granted) VALUES ($1::user_role, $2::permission_resource, $3::permission_action, true) ON CONFLICT DO NOTHING",
    )
    .bind(role)
    .bind(resource)
    .bind(action)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_shift(pool: &PgPool, branch_id: Uuid, teller_id: Uuid, status: &str) -> Uuid {
    // One drawer per shift: `idx_shifts_one_open_per_till` allows a single
    // open shift on a till, and the branch's default till would otherwise be
    // shared by every teller these tests seat at it.
    let till_id: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (org_id, branch_id, name) \
         SELECT org_id, id, $2 FROM branches WHERE id = $1 RETURNING id",
    )
    .bind(branch_id)
    .bind(format!("Till {}", &Uuid::new_v4().to_string()[..8]))
    .fetch_one(pool)
    .await
    .unwrap();
    let shift_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO shifts (id, branch_id, teller_id, till_id, status, opening_cash, closed_at) \
         VALUES ($1, $2, $3, $4, $5::shift_status, 10000, $6)",
    )
    .bind(shift_id)
    .bind(branch_id)
    .bind(teller_id)
    .bind(till_id)
    .bind(status)
    .bind((status != "open").then(chrono::Utc::now))
    .execute(pool)
    .await
    .unwrap();
    shift_id
}

/// A settled cash sale of `total` piastres with one line, the way the shift
/// tests seed theirs — straight into the tables, so this module's tests do
/// not ride on the order-creation lane.
async fn seed_order(
    pool: &PgPool,
    branch_id: Uuid,
    shift_id: Uuid,
    teller_id: Uuid,
    total: i32,
) -> (Uuid, Uuid) {
    let order_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, shift_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, $4, gen_random_uuid(), $5, 0, $5, 'completed', 1, 'cash', gen_random_uuid()::text)",
    )
    .bind(order_id)
    .bind(branch_id)
    .bind(teller_id)
    .bind(shift_id)
    .bind(total)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO order_payments (order_id, method, amount, is_cash) VALUES ($1, 'cash', $2, true)")
        .bind(order_id)
        .bind(total)
        .execute(pool)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO order_items (id, order_id, item_name, unit_price, quantity, line_total) VALUES ($1, $2, 'Koshari', $3, 2, $4)",
    )
    .bind(item_id)
    .bind(order_id)
    .bind(total / 2)
    .bind(total)
    .execute(pool)
    .await
    .unwrap();
    (order_id, item_id)
}

async fn order_status(pool: &PgPool, order_id: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn refund_rows(pool: &PgPool, order_id: Uuid) -> (i64, i64) {
    sqlx::query_as(
        "SELECT COUNT(*)::bigint, COALESCE(SUM(amount), 0)::bigint FROM order_refunds WHERE order_id = $1",
    )
    .bind(order_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A teller with an open shift and a 200-piastre cash sale in it, holding
/// `refunds create` + `refunds read`.
struct Till {
    org_id: Uuid,
    branch_id: Uuid,
    teller_id: Uuid,
    shift_id: Uuid,
    order_id: Uuid,
    item_id: Uuid,
    token: String,
}

async fn seed_till(pool: &PgPool) -> Till {
    let org_id = seed_org(pool).await;
    let branch_id = seed_branch(pool, org_id).await;
    let teller_id = seed_user(pool, org_id, "teller").await;
    grant(pool, "teller", "refunds", "create").await;
    grant(pool, "teller", "refunds", "read").await;
    let shift_id = seed_shift(pool, branch_id, teller_id, "open").await;
    let (order_id, item_id) = seed_order(pool, branch_id, shift_id, teller_id, 200).await;
    Till {
        org_id,
        branch_id,
        teller_id,
        shift_id,
        order_id,
        item_id,
        token: teller_token(teller_id, org_id, branch_id),
    }
}

macro_rules! app {
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

fn refund_body(order_id: Uuid, amount: i32) -> Value {
    json!({
        "order_id": order_id,
        "amount": amount,
        "method": "cash",
        "reason": "quality_issue",
    })
}

async fn post_refund<S>(app: &S, token: &str, body: &Value) -> actix_web::dev::ServiceResponse
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    test::call_service(
        app,
        test::TestRequest::post()
            .uri("/refunds")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(body)
            .to_request(),
    )
    .await
}

/// Assert the status and hand back the body — the body is read once, so a
/// failing assertion can still print it.
async fn body_with_status(resp: actix_web::dev::ServiceResponse, expected: u16) -> Value {
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    assert_eq!(
        status,
        expected,
        "unexpected status; body: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn issued_with_status(resp: actix_web::dev::ServiceResponse, expected: u16) -> RefundIssued {
    serde_json::from_value(body_with_status(resp, expected).await).unwrap()
}

// ── The five the lane asked for ───────────────────────────────

#[sqlx::test]
async fn partial_refund_leaves_the_status_alone(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let resp = post_refund(&app, &t.token, &refund_body(t.order_id, 50)).await;
    let issued = issued_with_status(resp, 201).await;
    assert_eq!(issued.refund.refund.amount, 50);
    assert!(issued.refund.refund.is_cash);
    assert_eq!(issued.refund.refund.shift_id, t.shift_id);
    assert_eq!(issued.refund.refund.issued_by, t.teller_id);
    assert_eq!(issued.totals.refunded_amount, 50);
    assert_eq!(issued.refundable_remaining, 150);
    assert_eq!(issued.order_status, "completed");
    assert_eq!(order_status(&pool, t.order_id).await, "completed");
}

#[sqlx::test]
async fn refunds_summing_to_the_total_flip_the_status(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let first = post_refund(&app, &t.token, &refund_body(t.order_id, 50)).await;
    assert_eq!(first.status(), 201);
    assert_eq!(order_status(&pool, t.order_id).await, "completed");

    let second = post_refund(&app, &t.token, &refund_body(t.order_id, 150)).await;
    let issued = issued_with_status(second, 201).await;
    // The trigger flipped it, and the response reports what the trigger did.
    assert_eq!(issued.order_status, "refunded");
    assert_eq!(issued.totals.refunded_amount, 200);
    assert_eq!(issued.totals.refund_count, 2);
    assert_eq!(issued.refundable_remaining, 0);
    assert_eq!(order_status(&pool, t.order_id).await, "refunded");

    // Nothing more can go back, and the status cannot be unsaid by a void.
    let third = post_refund(&app, &t.token, &refund_body(t.order_id, 1)).await;
    assert_eq!(third.status(), 409);
    let voided = sqlx::query(
        "UPDATE orders SET status = 'voided', voided_at = now(), voided_by = $2, void_reason = 'wrong_order' WHERE id = $1",
    )
    .bind(t.order_id)
    .bind(t.teller_id)
    .execute(&pool)
    .await;
    assert!(
        voided.is_err(),
        "a fully refunded order must not be voidable"
    );
}

#[sqlx::test]
async fn one_piastre_more_than_the_total_is_refused(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    // Outright.
    let resp = post_refund(&app, &t.token, &refund_body(t.order_id, 201)).await;
    assert_eq!(resp.status(), 400);
    let body: Value = test::read_body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap().contains("exceeds"),
        "{body}"
    );

    // And cumulatively: 150 back, then 51 more is one too many.
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 150))
            .await
            .status(),
        201
    );
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 51))
            .await
            .status(),
        400
    );
    assert_eq!(refund_rows(&pool, t.order_id).await, (1, 150));
    assert_eq!(order_status(&pool, t.order_id).await, "completed");

    // Exactly the remainder is fine.
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 50))
            .await
            .status(),
        201
    );
    assert_eq!(refund_rows(&pool, t.order_id).await, (2, 200));
}

#[sqlx::test]
async fn a_refund_outside_an_open_shift_is_refused(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    // The teller's shift closes; the sale stays on the books.
    sqlx::query("UPDATE shifts SET status = 'closed', closed_at = now() WHERE id = $1")
        .bind(t.shift_id)
        .execute(&pool)
        .await
        .unwrap();

    // Named explicitly: a closed drawer cannot lose money after the count.
    let mut body = refund_body(t.order_id, 50);
    body["shift_id"] = json!(t.shift_id);
    let resp = post_refund(&app, &t.token, &body).await;
    assert_eq!(resp.status(), 400);
    let err: Value = test::read_body_json(resp).await;
    assert!(
        err["error"].as_str().unwrap().contains("open shift"),
        "{err}"
    );

    // Left to the server to find: the teller has no open shift to issue from.
    let resp = post_refund(&app, &t.token, &refund_body(t.order_id, 50)).await;
    assert_eq!(resp.status(), 400);
    let err: Value = test::read_body_json(resp).await;
    assert!(
        err["error"].as_str().unwrap().contains("no open shift"),
        "{err}"
    );

    assert_eq!(refund_rows(&pool, t.order_id).await, (0, 0));

    // A fresh shift the next day can refund yesterday's sale — the refund
    // belongs to the drawer it leaves, not the one the sale was rung in.
    let today = seed_shift(&pool, t.branch_id, t.teller_id, "open").await;
    let resp = post_refund(&app, &t.token, &refund_body(t.order_id, 50)).await;
    let issued = issued_with_status(resp, 201).await;
    assert_eq!(issued.refund.refund.shift_id, today);
}

#[sqlx::test]
async fn replay_with_the_same_client_ref_is_idempotent(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let mut body = refund_body(t.order_id, 50);
    body["client_ref"] = json!(Uuid::new_v4());

    let first = post_refund(&app, &t.token, &body).await;
    assert_eq!(first.status(), 201);
    let first: RefundIssued = test::read_body_json(first).await;

    // Same queued op, flushed again: the original comes back, nothing is added.
    let again = post_refund(&app, &t.token, &body).await;
    assert_eq!(again.status(), 200);
    let again: RefundIssued = test::read_body_json(again).await;
    assert_eq!(again.refund.refund.id, first.refund.refund.id);
    assert_eq!(again.totals.refunded_amount, 50);
    assert_eq!(refund_rows(&pool, t.order_id).await, (1, 50));

    // A different key is a different refund.
    body["client_ref"] = json!(Uuid::new_v4());
    assert_eq!(post_refund(&app, &t.token, &body).await.status(), 201);
    assert_eq!(refund_rows(&pool, t.order_id).await, (2, 100));
}

// ── The rest of the contract ──────────────────────────────────

#[sqlx::test]
async fn cash_refunds_are_a_drawer_figure_keyed_on_the_issuing_shift(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let mut card = refund_body(t.order_id, 30);
    card["method"] = json!("card");
    assert_eq!(post_refund(&app, &t.token, &card).await.status(), 201);
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 50))
            .await
            .status(),
        201
    );

    // Only the cash leg leaves the drawer.
    assert_eq!(shift_cash_refunds(&pool, t.shift_id).await.unwrap(), 50);

    // Flipping the method's flag afterwards does not move the closed figure —
    // is_cash was snapshotted at issue.
    sqlx::query(
        "UPDATE org_payment_methods SET is_cash = false WHERE org_id = $1 AND name = 'cash'",
    )
    .bind(t.org_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(shift_cash_refunds(&pool, t.shift_id).await.unwrap(), 50);

    // The shift view carries both figures and the rows.
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/refunds/shift/{}", t.shift_id))
            .insert_header(("Authorization", format!("Bearer {}", t.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = test::read_body_json(resp).await;
    assert_eq!(body["refunded_amount"], 80);
    assert_eq!(body["refunded_cash"], 50);
    assert_eq!(body["refund_count"], 2);
    assert_eq!(body["refunds"].as_array().unwrap().len(), 2);
}

#[sqlx::test]
async fn the_order_view_lists_refunds_with_their_lines(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let mut body = refund_body(t.order_id, 100);
    body["reason"] = json!("wrong_order");
    body["note"] = json!("one koshari came out as pasta");
    body["lines"] = json!([{ "order_item_id": t.item_id, "quantity": 1, "amount": 100 }]);
    let resp = post_refund(&app, &t.token, &body).await;
    body_with_status(resp, 201).await;

    // The other unit of the line cannot be refunded twice over.
    body["lines"] = json!([{ "order_item_id": t.item_id, "quantity": 2, "amount": 100 }]);
    body.as_object_mut().unwrap().remove("client_ref");
    let resp = post_refund(&app, &t.token, &body).await;
    body_with_status(resp, 400).await;

    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/refunds/order/{}", t.order_id))
            .insert_header(("Authorization", format!("Bearer {}", t.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let view: Value = test::read_body_json(resp).await;
    assert_eq!(view["order_status"], "completed");
    assert_eq!(view["total_amount"], 200);
    assert_eq!(view["refunded_amount"], 100);
    assert_eq!(view["refundable_remaining"], 100);
    let refunds = view["refunds"].as_array().unwrap();
    assert_eq!(refunds.len(), 1);
    assert_eq!(refunds[0]["reason"], "wrong_order");
    assert!(
        refunds[0]["issued_by_name"]
            .as_str()
            .unwrap()
            .starts_with("Till "),
        "{}",
        refunds[0]["issued_by_name"]
    );
    let lines = refunds[0]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["item_name"], "Koshari");
    assert_eq!(lines[0]["quantity"], 1);
    assert_eq!(lines[0]["restock"], false);

    // And singly.
    let id = refunds[0]["id"].as_str().unwrap();
    let resp = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/refunds/{id}"))
            .insert_header(("Authorization", format!("Bearer {}", t.token)))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 200);
}

#[sqlx::test]
async fn a_refund_needs_the_refunds_permission_not_the_orders_one(pool: PgPool) {
    let app = app!(pool);
    let org_id = seed_org(&pool).await;
    let branch_id = seed_branch(&pool, org_id).await;
    let teller_id = seed_user(&pool, org_id, "teller").await;
    grant(&pool, "teller", "orders", "update").await; // enough to void — not to refund
    let shift_id = seed_shift(&pool, branch_id, teller_id, "open").await;
    let (order_id, _) = seed_order(&pool, branch_id, shift_id, teller_id, 200).await;

    let resp = post_refund(
        &app,
        &teller_token(teller_id, org_id, branch_id),
        &refund_body(order_id, 50),
    )
    .await;
    assert_eq!(resp.status(), 403);
    assert_eq!(refund_rows(&pool, order_id).await, (0, 0));
}

#[sqlx::test]
async fn a_voided_sale_has_no_money_to_return(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;
    sqlx::query(
        "UPDATE orders SET status = 'voided', voided_at = now(), voided_by = $2, void_reason = 'wrong_order' WHERE id = $1",
    )
    .bind(t.order_id)
    .bind(t.teller_id)
    .execute(&pool)
    .await
    .unwrap();

    let resp = post_refund(&app, &t.token, &refund_body(t.order_id, 50)).await;
    assert_eq!(resp.status(), 400);
    let err: Value = test::read_body_json(resp).await;
    assert!(err["error"].as_str().unwrap().contains("voided"), "{err}");
}

#[sqlx::test]
async fn other_needs_a_note_and_the_amount_must_be_money_out(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let mut body = refund_body(t.order_id, 50);
    body["reason"] = json!("other");
    assert_eq!(post_refund(&app, &t.token, &body).await.status(), 400);
    body["note"] = json!("  ");
    assert_eq!(post_refund(&app, &t.token, &body).await.status(), 400);
    body["note"] = json!("manager's call");
    assert_eq!(post_refund(&app, &t.token, &body).await.status(), 201);

    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 0))
            .await
            .status(),
        400
    );
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, -5))
            .await
            .status(),
        400
    );

    let mut unknown = refund_body(t.order_id, 10);
    unknown["method"] = json!("cheque");
    assert_eq!(post_refund(&app, &t.token, &unknown).await.status(), 400);
}

#[sqlx::test]
async fn a_teller_refunds_from_their_own_drawer_and_a_manager_may_pick_one(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    // Another teller's open drawer at the same branch.
    let other = seed_user(&pool, t.org_id, "teller").await;
    let other_shift = seed_shift(&pool, t.branch_id, other, "open").await;

    let mut body = refund_body(t.order_id, 50);
    body["shift_id"] = json!(other_shift);
    assert_eq!(post_refund(&app, &t.token, &body).await.status(), 403);

    // An org admin at the till may issue out of any open drawer (ruling 3:
    // no approval flow, the row says who it was).
    let admin = seed_user(&pool, t.org_id, "org_admin").await;
    grant(&pool, "org_admin", "refunds", "create").await;
    let resp = post_refund(&app, &org_admin_token(admin, t.org_id), &body).await;
    let issued = issued_with_status(resp, 201).await;
    assert_eq!(issued.refund.refund.shift_id, other_shift);
    assert_eq!(issued.refund.refund.issued_by, admin);

    // But not out of a drawer at another branch: the money left the branch
    // that took it.
    let elsewhere = seed_branch(&pool, t.org_id).await;
    let elsewhere_shift = seed_shift(&pool, elsewhere, admin, "open").await;
    body["shift_id"] = json!(elsewhere_shift);
    assert_eq!(
        post_refund(&app, &org_admin_token(admin, t.org_id), &body)
            .await
            .status(),
        400
    );
}

#[sqlx::test]
async fn another_orgs_order_is_not_found(pool: PgPool) {
    let app = app!(pool);
    let t = seed_till(&pool).await;

    let org_b = seed_org(&pool).await;
    let branch_b = seed_branch(&pool, org_b).await;
    let teller_b = seed_user(&pool, org_b, "teller").await;
    let shift_b = seed_shift(&pool, branch_b, teller_b, "open").await;

    let resp = post_refund(
        &app,
        &teller_token(teller_b, org_b, branch_b),
        &refund_body(t.order_id, 50),
    )
    .await;
    assert_eq!(resp.status(), 404);
    let _ = shift_b;
}

// ── The seam with the drawer ──────────────────────────────────

/// `compute_system_cash` is what a close snapshots and what the pre-close
/// screen shows. A cash refund is notes leaving that drawer, so it comes off;
/// and a sale that ends up FULLY refunded still put its notes in, so its
/// tender stays counted — otherwise a 200 sale refunded in cash would net the
/// drawer to −200 instead of 0. The revenue reports exclude a refunded sale
/// by status (it earned nothing); the drawer cannot, because the notes moved
/// twice and both moves are real.
#[sqlx::test]
async fn a_cash_refund_leaves_the_drawer_and_a_refunded_sale_still_entered_it(pool: PgPool) {
    use crate::shifts::handlers::compute_system_cash;
    let app = app!(pool);
    let t = seed_till(&pool).await;

    // Float 10000 + a 200 cash sale.
    assert_eq!(compute_system_cash(&pool, t.shift_id).await.unwrap(), 10200);

    // 50 back in cash: the drawer is 50 lighter. A card refund is not.
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 50))
            .await
            .status(),
        201
    );
    let mut card = refund_body(t.order_id, 30);
    card["method"] = json!("card");
    assert_eq!(post_refund(&app, &t.token, &card).await.status(), 201);
    assert_eq!(compute_system_cash(&pool, t.shift_id).await.unwrap(), 10150);
    assert_eq!(order_status(&pool, t.order_id).await, "completed");

    // The remaining 120 in cash flips the order to `refunded`. The sale's 200
    // still went INTO this drawer; 170 of it went back out of it in cash.
    assert_eq!(
        post_refund(&app, &t.token, &refund_body(t.order_id, 120))
            .await
            .status(),
        201
    );
    assert_eq!(order_status(&pool, t.order_id).await, "refunded");
    assert_eq!(
        compute_system_cash(&pool, t.shift_id).await.unwrap(),
        10200 - 170,
        "a fully refunded sale's cash tender stays in the drawer maths; only \
         the cash actually handed back comes off"
    );
    assert_eq!(shift_cash_refunds(&pool, t.shift_id).await.unwrap(), 170);
}

/// The refund comes off the drawer it was ISSUED from, which need not be the
/// drawer that made the sale. A manager refunding yesterday's sale from
/// today's shift makes today's drawer lighter and leaves yesterday's figure
/// alone.
#[sqlx::test]
async fn a_refund_from_another_shift_lightens_that_drawer_not_the_sales(pool: PgPool) {
    use crate::shifts::handlers::compute_system_cash;
    let app = app!(pool);
    let t = seed_till(&pool).await;
    let other_teller = seed_user(&pool, t.org_id, "teller").await;
    let other_shift = seed_shift(&pool, t.branch_id, other_teller, "open").await;
    let other_token = teller_token(other_teller, t.org_id, t.branch_id);

    let mut body = refund_body(t.order_id, 200);
    body["shift_id"] = json!(other_shift);
    assert_eq!(post_refund(&app, &other_token, &body).await.status(), 201);
    assert_eq!(order_status(&pool, t.order_id).await, "refunded");

    // The selling drawer keeps its 200; the refunding drawer is 200 down.
    assert_eq!(compute_system_cash(&pool, t.shift_id).await.unwrap(), 10200);
    assert_eq!(compute_system_cash(&pool, other_shift).await.unwrap(), 9800);
}
