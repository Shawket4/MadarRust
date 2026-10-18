//! The codes on the 409s a till's queued op meets through `/sync/replay`.
//!
//! The POS read every 409 on an op it called idempotent as "the server already
//! has this" and deleted the op, so a refused refund, void or fire vanished
//! from the till with nobody told. A replayed duplicate is a 200 almost
//! everywhere; these tests pin, op by op, that each 409 says with a code
//! whether it is a refusal (the work did not apply) or the rare "this exact op
//! is already here" — and that the status and the English text are exactly
//! what they were before the codes, so a till that reads no codes is unchanged.

use std::time::Duration;

use actix_web::{App, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole) -> String {
    create_token(&secret(), uid, Some(org), role, None, 24).unwrap()
}

/// An owner who works the till (owners hold every act), at one branch with
/// an open till, a menu item and a cash method.
struct Shop {
    org: Uuid,
    branch: Uuid,
    admin: Uuid,
    till: Uuid,
    item: Uuid,
    bearer: String,
}

async fn shop(pool: &PgPool) -> Shop {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(org)
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .unwrap();
    let branch = Uuid::new_v4();
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Branch')")
        .bind(branch)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let admin = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1, $2, 'Owner', $3, 'h', 'org_admin'::user_role)",
    )
    .bind(admin)
    .bind(org)
    .bind(format!("{admin}@t.com"))
    .execute(pool)
    .await
    .unwrap();
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, name, base_price) VALUES ($1, $2, 'Burger', 1000)",
    )
    .bind(item)
    .bind(org)
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
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) \
         VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(admin)
    .fetch_one(pool)
    .await
    .unwrap();
    Shop {
        org,
        branch,
        admin,
        till,
        item,
        bearer: token(admin, org, UserRole::OrgAdmin),
    }
}

macro_rules! app {
    ($pool:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::bookings::routes::configure)
                .configure(madar_rust::refunds::routes::configure)
                .configure(madar_rust::tickets::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    };
}

/// One queued op through the replay door: `(status, body)`.
async fn replay<S>(app: &S, bearer: &str, op: &Value) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/sync/replay")
            .insert_header(("Authorization", format!("Bearer {bearer}")))
            .set_json(op)
            .to_request(),
    )
    .await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A 409 with this code and exactly this English text.
#[track_caller]
fn refused(got: &(u16, Value), code: &str, error: &str) {
    assert_eq!(got.0, 409, "{}", got.1);
    assert_eq!(got.1["code"], code, "{}", got.1);
    assert_eq!(got.1["error"], error, "the text is what it always was");
}

/// Who sends a request: its `X-Madar-Client` and `User-Agent`.
type Caller = (Option<&'static str>, Option<&'static str>);

/// POS builds that ack every 409 on an idempotent op (before 0.15.0), as the
/// field sends them: 0.7.0+ name their version, the core before 0.7.0 only
/// its User-Agent.
const OLD_TILLS: &[Caller] = &[
    (Some("pos/0.14.1 (android)"), Some("madar-core/0.1.0")),
    (Some("pos/0.13.10 (android)"), None),
    (Some("pos/0.12.0 (ios)"), None),
    (Some("pos/0.7.0 (macos)"), None),
    (None, Some("madar-core/0.1.0")),
];

/// Callers that keep today's answers: 0.15.0 and later, and anyone the
/// headers do not prove old.
const TODAYS_CALLERS: &[Caller] = &[
    (Some("pos/0.15.0 (ios)"), Some("madar-core/0.1.0")),
    (Some("pos/0.16.0 (android)"), None),
    (Some("pos/kitchen/0.1.0 (ios)"), None),
    (Some("dashboard"), None),
    (None, Some("Dart/3.13 (dart:io)")),
];

async fn send<S>(app: &S, req: test::TestRequest, caller: Caller) -> (u16, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let mut req = req;
    if let Some(client) = caller.0 {
        req = req.insert_header(("X-Madar-Client", client));
    }
    if let Some(ua) = caller.1 {
        req = req.insert_header(("User-Agent", ua));
    }
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status().as_u16();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn replay_req(bearer: &str, op: &Value) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/sync/replay")
        .insert_header(("Authorization", format!("Bearer {bearer}")))
        .set_json(op)
}

/// A refusal through `/sync/replay`, sent by every kind of caller (it writes
/// nothing, so each sees the same state): today's 409 with this code and
/// text for 0.15.0+ and unknown callers, and for an old till a 422 with the
/// very same body, which every old build keeps as a stuck op instead of
/// acking it away.
async fn refused_for_each_till<S>(app: &S, bearer: &str, op: &Value, code: &str, error: &str)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let today = replay(app, bearer, op).await;
    refused(&today, code, error);
    for caller in TODAYS_CALLERS {
        let got = send(app, replay_req(bearer, op), *caller).await;
        assert_eq!(got, today, "{caller:?} keeps today's answer");
    }
    for caller in OLD_TILLS {
        let got = send(app, replay_req(bearer, op), *caller).await;
        assert_eq!(got.0, 422, "{caller:?}: {}", got.1);
        assert_eq!(got.1, today.1, "{caller:?}: the same body");
    }
}

/// A 409 that means "this exact op is already here": a 409 for every
/// caller, old tills included, because acking it is right.
async fn landed_for_each_till<S>(app: &S, bearer: &str, op: &Value, code: &str, error: &str)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let today = replay(app, bearer, op).await;
    refused(&today, code, error);
    for caller in TODAYS_CALLERS.iter().chain(OLD_TILLS) {
        let got = send(app, replay_req(bearer, op), *caller).await;
        assert_eq!(got, today, "{caller:?}: still the 409");
    }
}

/// A live route answers an old till's refusal exactly as before: only the
/// replay door rewrites it.
async fn live_unchanged<S>(app: &S, req: impl Fn() -> test::TestRequest, code: &str, error: &str)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let today = send(app, req(), (None, None)).await;
    refused(&today, code, error);
    for caller in OLD_TILLS.iter().chain(TODAYS_CALLERS) {
        assert_eq!(send(app, req(), *caller).await, today, "{caller:?}");
    }
}

fn fire(s: &Shop, key: Uuid, booking: Option<Uuid>) -> Value {
    json!({
        "op": "fire_open_ticket",
        "teller_id": s.admin,
        "request": {
            "branch_id": s.branch,
            "idempotency_key": key,
            "round_idempotency_key": Uuid::new_v4(),
            "booking_id": booking,
            "items": [{ "menu_item_id": s.item, "quantity": 1 }]
        }
    })
}

async fn fired<S>(app: &S, s: &Shop) -> Uuid
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let r = replay(app, &s.bearer, &fire(s, Uuid::new_v4(), None)).await;
    assert_eq!(r.0, 201, "{}", r.1);
    Uuid::parse_str(r.1["id"].as_str().unwrap()).unwrap()
}

async fn settle<S>(app: &S, s: &Shop, ticket: Uuid)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let op = json!({
        "op": "settle_open_ticket", "teller_id": s.admin, "ticket_id": ticket,
        "request": { "shift_id": s.till, "payment_method": "cash" }
    });
    let r = replay(app, &s.bearer, &op).await;
    assert!(r.0 == 200 || r.0 == 201, "{}", r.1);
}

async fn order(pool: &PgPool, s: &Shop, total: i32) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO orders (branch_id, teller_id, till_id, idempotency_key, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, gen_random_uuid(), $4, 0, $4, 'completed', \
                 (SELECT COALESCE(MAX(order_number), 0) + 1 FROM orders WHERE till_id = $3), 'cash', gen_random_uuid()::text) RETURNING id",
    )
    .bind(s.branch)
    .bind(s.admin)
    .bind(s.till)
    .bind(total)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn refund(s: &Shop, order: Uuid, amount: i32, cref: Uuid) -> Value {
    json!({
        "op": "refund_order", "teller_id": s.admin,
        "request": {
            "order_id": order, "shift_id": s.till, "amount": amount, "method": "cash",
            "reason": "quality_issue", "client_ref": cref
        }
    })
}

/// The money case. A refund queued on one till, while another refunded the
/// whole sale: the queued one did not apply, and the cash it handed back is
/// on no record. Its own retry is still a 200; the other 409 is a refusal.
#[sqlx::test]
async fn a_refund_of_a_sale_refunded_elsewhere_is_a_refusal(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let sale = order(&pool, &s, 300).await;

    let first = refund(&s, sale, 300, Uuid::new_v4());
    assert_eq!(replay(&app, &s.bearer, &first).await.0, 201);
    assert_eq!(
        replay(&app, &s.bearer, &first).await.0,
        200,
        "the same refund again is the original"
    );

    let other = refund(&s, sale, 100, Uuid::new_v4());
    refused_for_each_till(
        &app,
        &s.bearer,
        &other,
        "ORDER_FULLY_REFUNDED",
        "Conflict: This order has already been refunded in full",
    )
    .await;
    live_unchanged(
        &app,
        || {
            test::TestRequest::post()
                .uri("/refunds")
                .insert_header(("Authorization", format!("Bearer {}", s.bearer)))
                .set_json(&other["request"])
        },
        "ORDER_FULLY_REFUNDED",
        "Conflict: This order has already been refunded in full",
    )
    .await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM order_refunds WHERE order_id = $1")
        .bind(sale)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "the refused refunds wrote nothing");
}

/// A sale that has given money back cannot be voided. The POS read this 409
/// as "already voided" and dropped the void: the server never had a 409 for
/// that (an already-voided sale is a 200).
#[sqlx::test]
async fn a_void_of_a_refunded_sale_is_a_refusal_and_a_repeat_void_is_a_200(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let sale = order(&pool, &s, 300).await;
    assert_eq!(
        replay(&app, &s.bearer, &refund(&s, sale, 100, Uuid::new_v4()))
            .await
            .0,
        201
    );

    let void = |id: Uuid| json!({ "op": "void_order", "teller_id": s.admin, "order_id": id, "request": { "reason": "wrong_order" } });
    refused_for_each_till(
        &app,
        &s.bearer,
        &void(sale),
        "ORDER_REFUNDED",
        "Conflict: 100 has already been refunded against this sale, so it cannot be voided — refund the remainder instead.",
    )
    .await;

    let clean = order(&pool, &s, 300).await;
    assert_eq!(replay(&app, &s.bearer, &void(clean)).await.0, 200);
    assert_eq!(
        replay(&app, &s.bearer, &void(clean)).await.0,
        200,
        "voiding a voided sale is a 200, never a 409"
    );
}

/// Two corrections of one pay-out: the second did not apply.
#[sqlx::test]
async fn a_second_correction_of_one_movement_is_a_refusal(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let cash = |request: Value| json!({ "op": "cash_movement", "teller_id": s.admin, "till_id": s.till, "request": request });
    let out = replay(
        &app,
        &s.bearer,
        &cash(json!({ "amount": -500, "note": "milk", "client_ref": Uuid::new_v4() })),
    )
    .await;
    assert_eq!(out.0, 201, "{}", out.1);
    let moved = out.1["id"].clone();
    let undo = |cref: Uuid| {
        cash(
            json!({ "amount": 500, "kind": "correction", "corrects_id": moved, "note": "undo", "client_ref": cref }),
        )
    };
    let first = undo(Uuid::new_v4());
    assert_eq!(replay(&app, &s.bearer, &first).await.0, 201);
    assert_eq!(
        replay(&app, &s.bearer, &first).await.0,
        200,
        "its own retry"
    );
    refused_for_each_till(
        &app,
        &s.bearer,
        &undo(Uuid::new_v4()),
        "CASH_ALREADY_CORRECTED",
        "Conflict: That movement has already been corrected",
    )
    .await;
}

/// A paid bill cannot be voided; a voided one voids again as a 200.
#[sqlx::test]
async fn a_void_of_a_paid_bill_is_a_refusal(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let void = |t: Uuid| json!({ "op": "void_open_ticket", "teller_id": s.admin, "ticket_id": t, "request": { "reason": "wrong_order" } });

    let paid = fired(&app, &s).await;
    settle(&app, &s, paid).await;
    refused_for_each_till(
        &app,
        &s.bearer,
        &void(paid),
        "TICKET_SETTLED",
        "Conflict: Cannot void a settled ticket",
    )
    .await;
    live_unchanged(
        &app,
        || {
            test::TestRequest::post()
                .uri(&format!("/open-tickets/{paid}/void"))
                .insert_header(("Authorization", format!("Bearer {}", s.bearer)))
                .set_json(json!({ "reason": "wrong_order" }))
        },
        "TICKET_SETTLED",
        "Conflict: Cannot void a settled ticket",
    )
    .await;

    let torn = fired(&app, &s).await;
    assert_eq!(replay(&app, &s.bearer, &void(torn)).await.0, 200);
    assert_eq!(
        replay(&app, &s.bearer, &void(torn)).await.0,
        200,
        "already voided is a 200"
    );
}

/// A line void meets three 409s, and the code says which: the bill was paid
/// with the line still on it (a refusal), the bill was paid AFTER this void
/// took the line off (the void is on record), or the bill was voided (no line
/// of it is owed for).
#[sqlx::test]
async fn a_line_void_says_whether_it_is_on_record(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let line_of = |t: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM open_ticket_items WHERE open_ticket_id = $1 LIMIT 1",
            )
            .bind(t)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let void_line = |t: Uuid, line: Uuid| json!({ "op": "void_ticket_line", "teller_id": s.admin, "ticket_id": t, "item_id": line, "request": { "reason": "quality_issue" } });
    const SETTLED: &str = "Conflict: That bill is settled — a paid line is a refund, not a void";

    // Paid with the line on it: the void did not apply.
    let paid = fired(&app, &s).await;
    let line = line_of(paid).await;
    settle(&app, &s, paid).await;
    refused_for_each_till(
        &app,
        &s.bearer,
        &void_line(paid, line),
        "TICKET_SETTLED",
        SETTLED,
    )
    .await;

    // Voided first (its answer lost), the bill paid, the retry arrives.
    let kept = fired(&app, &s).await;
    // A second line, so the bill still has something to pay for.
    let round = json!({
        "op": "add_ticket_round", "teller_id": s.admin, "ticket_id": kept,
        "request": { "idempotency_key": Uuid::new_v4(), "items": [{ "menu_item_id": s.item, "quantity": 1 }] }
    });
    assert!(replay(&app, &s.bearer, &round).await.0 < 300);
    let line = line_of(kept).await;
    assert_eq!(replay(&app, &s.bearer, &void_line(kept, line)).await.0, 200);
    settle(&app, &s, kept).await;
    landed_for_each_till(
        &app,
        &s.bearer,
        &void_line(kept, line),
        "LINE_ALREADY_VOIDED",
        SETTLED,
    )
    .await;

    // The whole bill torn up.
    let torn = fired(&app, &s).await;
    let line = line_of(torn).await;
    let void = json!({ "op": "void_open_ticket", "teller_id": s.admin, "ticket_id": torn, "request": { "reason": "wrong_order" } });
    assert_eq!(replay(&app, &s.bearer, &void).await.0, 200);
    landed_for_each_till(
        &app,
        &s.bearer,
        &void_line(torn, line),
        "TICKET_VOIDED",
        "Conflict: That bill was voided",
    )
    .await;
}

/// The sweep rolls a late party to no-show while the waiter's seat and fire
/// are still queued. The seat is refused, with a code. The FIRE is not: the
/// party is at the table and ate, so the replay fires the ticket without the
/// booking link (as it drops a table it cannot claim) and the booking stays a
/// no-show. See `a_replayed_fire_drops_a_booking_it_cannot_use` below.
#[sqlx::test]
async fn a_booking_that_is_no_longer_active_refuses_with_a_code(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let book = || async {
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/bookings")
                .insert_header(("Authorization", format!("Bearer {}", s.bearer)))
                .set_json(json!({
                    "branch_id": s.branch, "party_size": 2,
                    "starts_at": chrono::Utc::now() + chrono::Duration::minutes(5),
                    "guest_name": "Ahmed", "guest_phone": "01000000001", "force": true, "table_ids": []
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 201);
        let b: Value = test::read_body_json(resp).await;
        Uuid::parse_str(b["id"].as_str().unwrap()).unwrap()
    };
    let no_show =
        |b: Uuid| json!({ "op": "no_show_booking", "teller_id": s.admin, "booking_id": b });
    let seat = |b: Uuid| json!({ "op": "seat_booking", "teller_id": s.admin, "booking_id": b, "request": {} });

    let late = book().await;
    assert_eq!(replay(&app, &s.bearer, &no_show(late)).await.0, 200);
    assert_eq!(
        replay(&app, &s.bearer, &no_show(late)).await.0,
        200,
        "a no-show again is a 200"
    );
    refused_for_each_till(
        &app,
        &s.bearer,
        &seat(late),
        "BOOKING_NOT_ACTIVE",
        "Conflict: Booking is no_show",
    )
    .await;
    let key = Uuid::new_v4();
    let r = replay(&app, &s.bearer, &fire(&s, key, Some(late))).await;
    assert_eq!(r.0, 201, "the fire lands, without the booking: {}", r.1);
    assert_eq!(r.1["booking_id"], Value::Null, "{}", r.1);

    let sat = book().await;
    assert_eq!(replay(&app, &s.bearer, &seat(sat)).await.0, 200);
    assert_eq!(
        replay(&app, &s.bearer, &seat(sat)).await.0,
        200,
        "seated again is a 200"
    );
    refused_for_each_till(
        &app,
        &s.bearer,
        &no_show(sat),
        "BOOKING_SEATED",
        "Conflict: The party was seated — cancel the booking if they left",
    )
    .await;
    live_unchanged(
        &app,
        || {
            test::TestRequest::post()
                .uri(&format!("/bookings/{sat}/no-show"))
                .insert_header(("Authorization", format!("Bearer {}", s.bearer)))
        },
        "BOOKING_SEATED",
        "Conflict: The party was seated — cancel the booking if they left",
    )
    .await;

    // Seated already, and the queued seat names other tables.
    let table: Uuid = sqlx::query_scalar(
        "INSERT INTO branch_tables (org_id, branch_id, label, seats) VALUES ($1, $2, 'T7', 4) RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    let reseat = json!({ "op": "seat_booking", "teller_id": s.admin, "booking_id": sat, "request": { "table_ids": [table] } });
    refused_for_each_till(
        &app,
        &s.bearer,
        &reseat,
        "BOOKING_SEATED_ELSEWHERE",
        "Conflict: The party is already seated — move them on the floor",
    )
    .await;
}

/// A booking at `branch`, through the live route (`force`: no slot rules).
async fn book_at<S>(app: &S, s: &Shop, branch: Uuid) -> Uuid
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(
        app,
        test::TestRequest::post()
            .uri("/bookings")
            .insert_header(("Authorization", format!("Bearer {}", s.bearer)))
            .set_json(json!({
                "branch_id": branch, "party_size": 2,
                "starts_at": chrono::Utc::now() + chrono::Duration::minutes(5),
                "guest_name": "Ahmed", "guest_phone": "01000000001", "force": true, "table_ids": []
            }))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let b: Value = test::read_body_json(resp).await;
    Uuid::parse_str(b["id"].as_str().unwrap()).unwrap()
}

async fn booking_status(pool: &PgPool, b: Uuid) -> String {
    sqlx::query_scalar("SELECT status::text FROM bookings WHERE id = $1")
        .bind(b)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `(ticket count, its booking_id)` for one fire's key.
async fn tickets_for(pool: &PgPool, key: Uuid) -> (i64, Option<Uuid>) {
    sqlx::query_as(
        "SELECT COUNT(*), MAX(booking_id::text)::uuid FROM open_tickets WHERE idempotency_key = $1",
    )
    .bind(key)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The sweep moved a late party to no-show (or the fire names another
/// branch's booking) before the waiter's offline fire arrived. The replay
/// fires the ticket WITHOUT the booking link rather than losing the bill: the
/// ticket exists, the booking is untouched, a duplicate is still one ticket,
/// and the cashier's queued settle afterwards goes through.
#[sqlx::test]
async fn a_replayed_fire_drops_a_booking_it_cannot_use(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;

    // No-show.
    let late = book_at(&app, &s, s.branch).await;
    let ns = json!({ "op": "no_show_booking", "teller_id": s.admin, "booking_id": late });
    assert_eq!(replay(&app, &s.bearer, &ns).await.0, 200);
    let key = Uuid::new_v4();
    let op = fire(&s, key, Some(late));
    let r = replay(&app, &s.bearer, &op).await;
    assert_eq!(r.0, 201, "{}", r.1);
    assert_eq!(r.1["booking_id"], Value::Null, "{}", r.1);
    let ticket = Uuid::parse_str(r.1["id"].as_str().unwrap()).unwrap();
    assert_eq!(tickets_for(&pool, key).await, (1, None), "fired, unlinked");
    assert_eq!(
        booking_status(&pool, late).await,
        "no_show",
        "no auto-seating"
    );

    // The same fire again (the till's retry): one ticket.
    let again = replay(&app, &s.bearer, &op).await;
    assert_eq!(again.0, 200, "{}", again.1);
    assert_eq!(again.1["id"], ticket.to_string());
    assert_eq!(tickets_for(&pool, key).await, (1, None), "still one ticket");

    // The cashier's queued settle afterwards.
    settle(&app, &s, ticket).await;
    let status: String = sqlx::query_scalar("SELECT status::text FROM open_tickets WHERE id = $1")
        .bind(ticket)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "settled");
    assert_eq!(booking_status(&pool, late).await, "no_show");

    // A booking at another branch of the org.
    let other: Uuid =
        sqlx::query_scalar("INSERT INTO branches (org_id, name) VALUES ($1, 'Other') RETURNING id")
            .bind(s.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    let elsewhere = book_at(&app, &s, other).await;
    let key = Uuid::new_v4();
    let r = replay(&app, &s.bearer, &fire(&s, key, Some(elsewhere))).await;
    assert_eq!(r.0, 201, "{}", r.1);
    assert_eq!(r.1["booking_id"], Value::Null, "{}", r.1);
    assert_eq!(tickets_for(&pool, key).await, (1, None));
    assert_eq!(
        booking_status(&pool, elsewhere).await,
        "confirmed",
        "untouched"
    );
    let ticket = Uuid::parse_str(r.1["id"].as_str().unwrap()).unwrap();
    settle(&app, &s, ticket).await;

    // A usable booking is still linked and seated on replay.
    let ok = book_at(&app, &s, s.branch).await;
    let key = Uuid::new_v4();
    let r = replay(&app, &s.bearer, &fire(&s, key, Some(ok))).await;
    assert_eq!(r.0, 201, "{}", r.1);
    assert_eq!(r.1["booking_id"], ok.to_string());
    assert_eq!(booking_status(&pool, ok).await, "seated");
}

/// The LIVE fire keeps refusing: a teller who is online is told at once.
#[sqlx::test]
async fn a_live_fire_under_a_dead_booking_is_still_a_409(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let live = |key: Uuid, b: Uuid| {
        test::TestRequest::post()
            .uri("/open-tickets")
            .insert_header(("Authorization", format!("Bearer {}", s.bearer)))
            .set_json(json!({
                "branch_id": s.branch,
                "idempotency_key": key,
                "round_idempotency_key": Uuid::new_v4(),
                "booking_id": b,
                "items": [{ "menu_item_id": s.item, "quantity": 1 }]
            }))
            .to_request()
    };
    let read = |resp: actix_web::dev::ServiceResponse| async move {
        let status = resp.status().as_u16();
        let body: Value = test::read_body_json(resp).await;
        (status, body)
    };

    let late = book_at(&app, &s, s.branch).await;
    let ns = json!({ "op": "no_show_booking", "teller_id": s.admin, "booking_id": late });
    assert_eq!(replay(&app, &s.bearer, &ns).await.0, 200);
    let key = Uuid::new_v4();
    refused(
        &read(test::call_service(&app, live(key, late)).await).await,
        "BOOKING_NOT_ACTIVE",
        "Conflict: Booking is no_show",
    );
    assert_eq!(
        tickets_for(&pool, key).await,
        (0, None),
        "live refusal: nothing"
    );

    let other: Uuid =
        sqlx::query_scalar("INSERT INTO branches (org_id, name) VALUES ($1, 'Other') RETURNING id")
            .bind(s.org)
            .fetch_one(&pool)
            .await
            .unwrap();
    let elsewhere = book_at(&app, &s, other).await;
    let key = Uuid::new_v4();
    refused(
        &read(test::call_service(&app, live(key, elsewhere)).await).await,
        "BOOKING_OTHER_BRANCH",
        "Conflict: Booking belongs to another branch",
    );
    assert_eq!(tickets_for(&pool, key).await, (0, None));
    assert_eq!(booking_status(&pool, late).await, "no_show");
    assert_eq!(booking_status(&pool, elsewhere).await, "confirmed");
}

/// The one 409 that means "already here": two copies of one fire (the till's
/// retry and its LAN peer's backup) replayed at once. Both pass the key check;
/// the second loses on the key. Made deterministic by holding the first copy's
/// key in an open transaction while the second arrives.
#[sqlx::test]
async fn a_fire_that_loses_the_race_to_its_own_copy_says_it_landed(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let first = fired(&app, &s).await;
    let key = Uuid::new_v4();

    let mut holder = pool.begin().await.unwrap();
    sqlx::query("UPDATE open_tickets SET idempotency_key = $2 WHERE id = $1")
        .bind(first)
        .bind(key)
        .execute(&mut *holder)
        .await
        .unwrap();
    let second = fire(&s, key, None);
    // Sent by an old till: this 409 is the one it is right to ack, so it
    // stays a 409 for it too.
    let (got, ()) = tokio::join!(
        send(&app, replay_req(&s.bearer, &second), OLD_TILLS[0]),
        async {
            tokio::time::sleep(Duration::from_millis(700)).await;
            holder.commit().await.unwrap();
        }
    );
    assert_eq!(got.0, 409, "{}", got.1);
    assert_eq!(got.1["code"], "REPLAY_ALREADY_APPLIED", "{}", got.1);
    let text = got.1["error"].as_str().unwrap();
    assert!(
        text.starts_with("Database error: ") && text.contains("duplicate key"),
        "the plain unique violation's text: {text}"
    );
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM open_tickets WHERE idempotency_key = $1")
        .bind(key)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "one bill for the key");
}

/// The same race for a transfer wish, on the wish's own id.
#[sqlx::test]
async fn a_transfer_that_loses_the_race_to_its_own_copy_says_it_landed(pool: PgPool) {
    let app = app!(pool);
    let s = shop(&pool).await;
    let table: Uuid = sqlx::query_scalar(
        "INSERT INTO branch_tables (org_id, branch_id, label, seats) VALUES ($1, $2, 'T9', 4) RETURNING id",
    )
    .bind(s.org)
    .bind(s.branch)
    .fetch_one(&pool)
    .await
    .unwrap();
    let ticket = fired(&app, &s).await;
    let id = Uuid::new_v4();

    let mut holder = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO table_transfer_requests \
            (id, org_id, branch_id, occupant_kind, occupant_id, target_table_id, requested_by) \
         VALUES ($1, $2, $3, 'open_ticket', $4, $5, $6)",
    )
    .bind(id)
    .bind(s.org)
    .bind(s.branch)
    .bind(ticket)
    .bind(table)
    .bind(s.admin)
    .execute(&mut *holder)
    .await
    .unwrap();
    let op = json!({
        "op": "create_table_transfer", "teller_id": s.admin,
        "request": { "id": id, "branch_id": s.branch, "occupant_kind": "open_ticket", "occupant_id": ticket, "target_table_id": table }
    });
    let (got, ()) = tokio::join!(replay(&app, &s.bearer, &op), async {
        tokio::time::sleep(Duration::from_millis(700)).await;
        holder.commit().await.unwrap();
    });
    assert_eq!(got.0, 409, "{}", got.1);
    assert_eq!(got.1["code"], "REPLAY_ALREADY_APPLIED", "{}", got.1);

    // A different wish for the same party is a refusal, as before.
    let other = json!({
        "op": "create_table_transfer", "teller_id": s.admin,
        "request": { "id": Uuid::new_v4(), "branch_id": s.branch, "occupant_kind": "open_ticket", "occupant_id": ticket, "target_table_id": table }
    });
    let r = replay(&app, &s.bearer, &other).await;
    assert_eq!(r.0, 409);
    assert_eq!(r.1["code"], "TRANSFER_EXISTS");
}
