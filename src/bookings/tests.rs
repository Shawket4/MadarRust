//! Bookings end to end: best-fit assignment and the DB overlap guard, the host
//! edit/cancel paths, the public OTP-gated flow with its manage link, seating
//! through a fired ticket, the realtime topic, the sweep, and offline replay.

use actix_web::http::StatusCode;
use actix_web::{App, test, web};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use super::model::BookingView;
use crate::auth::jwt::{JwtSecret, create_token};
use crate::models::UserRole;
use crate::realtime::event::Topic;
use crate::realtime::hub::BranchEventHub;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn token(uid: Uuid, org: Uuid, role: UserRole, branch: Option<Uuid>) -> String {
    create_token(&secret(), uid, Some(org), role, branch, 24).unwrap()
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
    sqlx::query("INSERT INTO branches (id, org_id, name) VALUES ($1, $2, 'Maadi')")
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
async fn seed_table(pool: &PgPool, org: Uuid, branch: Uuid, label: &str, seats: i16) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO branch_tables (id, org_id, branch_id, label, seats) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(org)
    .bind(branch)
    .bind(label)
    .bind(seats)
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
async fn perms(pool: &PgPool) {
    crate::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}
async fn enable_public(pool: &PgPool, org: Uuid, branch: Uuid, require_otp: bool) {
    sqlx::query(
        "INSERT INTO branch_booking_settings (branch_id, org_id, enabled, require_otp, lead_time_minutes) \
         VALUES ($1, $2, true, $3, 60)",
    )
    .bind(branch)
    .bind(org)
    .bind(require_otp)
    .execute(pool)
    .await
    .unwrap();
}

macro_rules! app {
    ($pool:expr, $hub:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new($hub.clone()))
                .configure(crate::bookings::routes::configure)
                .configure(crate::tickets::routes::configure)
                .configure(crate::kitchen::routes::configure)
                .configure(crate::reservations::routes::configure)
                .configure(crate::sync::routes::configure),
        )
        .await
    };
}

fn auth(req: test::TestRequest, token: &str) -> test::TestRequest {
    req.insert_header(("Authorization", format!("Bearer {token}")))
}

async fn send<S>(app: &S, req: test::TestRequest) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
        >,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// Tomorrow 19:00 Cairo, which is a slot on the default 12:00–23:00 window.
fn tomorrow_1900() -> DateTime<Utc> {
    use chrono::TimeZone;
    let tz = chrono_tz::Africa::Cairo;
    let d = (Utc::now().with_timezone(&tz) + Duration::days(1)).date_naive();
    tz.from_local_datetime(&d.and_hms_opt(19, 0, 0).unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

fn create_body(branch: Uuid, party: i32, at: DateTime<Utc>) -> Value {
    json!({
        "branch_id": branch, "party_size": party, "starts_at": at,
        "guest_name": "Ahmed", "guest_phone": "01000000001"
    })
}

// ── Host: assignment + overlap ────────────────────────────────────────────────

#[sqlx::test]
async fn host_booking_auto_assigns_best_fit_and_refuses_when_full(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let t2 = seed_table(&pool, org, branch, "T1", 2).await;
    let t4 = seed_table(&pool, org, branch, "T2", 4).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let tok = token(admin, org, UserRole::OrgAdmin, None);
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let at = tomorrow_1900();

    // Party of 3 → the 4-top, not the 2-top.
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(create_body(branch, 3, at)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["table_ids"], json!([t4]));
    assert_eq!(b["status"], "confirmed");
    assert_eq!(b["needs_table"], false);
    assert_eq!(b["guest_phone"], "201000000001", "phone normalized");
    assert!(b["held_from"].as_str().is_some());

    // Another 3 at the same time: only the 2-top is left → conflict.
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(create_body(branch, 3, at)),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");

    // A couple fits the 2-top.
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(create_body(branch, 2, at)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["table_ids"], json!([t2]));

    // Nothing left; the host may still force it in, flagged as needing a table.
    let mut forced = create_body(branch, 2, at);
    forced["force"] = json!(true);
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(forced),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["needs_table"], true);
    assert_eq!(b["table_ids"], json!([]));

    // Two hours later both tables are free again (90-minute default duration).
    let later = at + Duration::hours(2);
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok)
            .set_json(create_body(branch, 4, later)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(b["table_ids"], json!([t4]));

    // The day list carries all four, ordered by start.
    let date = at.with_timezone(&chrono_tz::Africa::Cairo).date_naive();
    let (st, list) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/bookings?branch_id={branch}&date={date}")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 4);
}

#[sqlx::test]
async fn the_database_refuses_overlapping_claims_on_one_table(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let t = seed_table(&pool, org, branch, "T1", 4).await;
    let at = tomorrow_1900();
    let mk = |off: i64| {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, guest_phone) \
             VALUES ($1, $2, 2, $3, $4, 'A', '2010') RETURNING id",
        )
        .bind(org)
        .bind(branch)
        .bind(at + Duration::minutes(off))
        .bind(at + Duration::minutes(off + 90))
    };
    let b1: Uuid = mk(0).fetch_one(&pool).await.unwrap();
    let b2: Uuid = mk(30).fetch_one(&pool).await.unwrap();
    let b3: Uuid = mk(90).fetch_one(&pool).await.unwrap();
    let claim = |b: Uuid| {
        sqlx::query("INSERT INTO booking_tables (booking_id, table_id) VALUES ($1, $2)")
            .bind(b)
            .bind(t)
    };
    claim(b1).execute(&pool).await.unwrap();
    let err = claim(b2).execute(&pool).await.unwrap_err();
    let code = match &err {
        sqlx::Error::Database(d) => d.code().map(|c| c.to_string()),
        _ => None,
    };
    assert_eq!(code.as_deref(), Some("23P01"), "exclusion violation: {err}");
    // Back-to-back is fine ([) ranges).
    claim(b3).execute(&pool).await.unwrap();
    // Cancelling b1 releases its claim, so b2 can now take the table.
    sqlx::query("UPDATE bookings SET status = 'cancelled' WHERE id = $1")
        .bind(b1)
        .execute(&pool)
        .await
        .unwrap();
    let active: bool =
        sqlx::query_scalar("SELECT active FROM booking_tables WHERE booking_id = $1")
            .bind(b1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!active, "trigger deactivated the cancelled claim");
    // (b2 still collides with b3's back-to-back claim; a party in b1's exact
    // window now fits.)
    let b4: Uuid = mk(0).fetch_one(&pool).await.unwrap();
    claim(b4).execute(&pool).await.unwrap();
}

#[sqlx::test]
async fn host_edit_moves_tables_and_cancel_frees_them(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let t2 = seed_table(&pool, org, branch, "T1", 2).await;
    let t6 = seed_table(&pool, org, branch, "T2", 6).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let tok = token(admin, org, UserRole::OrgAdmin, None);
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let at = tomorrow_1900();

    let (_, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(create_body(branch, 2, at)),
    )
    .await;
    let id = b["id"].as_str().unwrap().to_string();
    assert_eq!(b["table_ids"], json!([t2]));

    // Party grows to 5 → the 2-top no longer fits → re-picked onto the 6-top.
    let (st, b) = send(
        &app,
        auth(
            test::TestRequest::patch().uri(&format!("/bookings/{id}")),
            &tok,
        )
        .set_json(json!({"party_size": 5})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["table_ids"], json!([t6]));

    // Explicit reassignment to both tables.
    let (st, b) = send(
        &app,
        auth(
            test::TestRequest::patch().uri(&format!("/bookings/{id}")),
            &tok,
        )
        .set_json(json!({"table_ids": [t2, t6]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["table_ids"].as_array().unwrap().len(), 2);

    // A second party cannot take either table now…
    let (st, _) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(create_body(branch, 2, at)),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    // …until the first is cancelled.
    let (st, b) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/bookings/{id}/cancel")),
            &tok,
        )
        .set_json(json!({"reason": "guest called"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "cancelled");
    assert_eq!(b["cancelled_by"], "host");
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(create_body(branch, 2, at)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // A cancelled booking cannot be edited or cancelled again.
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::patch().uri(&format!("/bookings/{id}")),
            &tok,
        )
        .set_json(json!({"notes": "x"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
}

// ── Public flow ───────────────────────────────────────────────────────────────

#[sqlx::test]
async fn public_guest_books_manages_and_cancels(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_table(&pool, org, branch, "T1", 4).await;
    enable_public(&pool, org, branch, false).await;
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);

    let (st, branches) = send(
        &app,
        test::TestRequest::get().uri(&format!("/public/booking-branches?org_id={org}")),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(branches[0]["id"], json!(branch));

    let (st, info) = send(
        &app,
        test::TestRequest::get().uri(&format!("/public/branches/{branch}/booking-info")),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{info}");
    assert_eq!(info["enabled"], true);
    assert_eq!(info["require_otp"], false);
    assert_eq!(info["timezone"], "Africa/Cairo");

    let at = tomorrow_1900();
    let date = at.with_timezone(&chrono_tz::Africa::Cairo).date_naive();
    let (st, slots) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/branches/{branch}/booking-slots?date={date}&party_size=2"
        )),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{slots}");
    let slot = slots["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["starts_at"] == json!(at))
        .expect("19:00 is a slot");
    assert_eq!(slot["available"], true);
    assert!(
        slot.get("table_ids").is_none(),
        "no table ids leak to guests"
    );

    // Party outside the online limits is refused with a clear message.
    let (st, b) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/branches/{branch}/booking-slots?date={date}&party_size=40"
        )),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");

    let body = json!({"branch_id": branch, "starts_at": at, "party_size": 2, "guest_name": "Sara", "phone": "01011111111", "locale": "ar"});
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/bookings")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let token = b["manage_token"].as_str().unwrap().to_string();
    assert_eq!(b["status"], "confirmed");
    assert_eq!(b["can_modify"], true);
    assert!(b.get("table_ids").is_none() && b.get("guest_phone").is_none());

    // The slot is gone for the next guest.
    let (_, slots) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/branches/{branch}/booking-slots?date={date}&party_size=2"
        )),
    )
    .await;
    let slot = slots["slots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["starts_at"] == json!(at))
        .unwrap();
    assert_eq!(slot["available"], false);
    let (st, b2) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/bookings")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{b2}");

    // An off-grid time is refused.
    let mut off = body.clone();
    off["starts_at"] = json!(at + Duration::minutes(7));
    let (st, _) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/bookings")
            .set_json(&off),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Manage: view, move later, cancel.
    let (st, m) = send(
        &app,
        test::TestRequest::get().uri(&format!("/public/bookings/{token}")),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(m["guest_name"], "Sara");
    let (st, m) = send(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/public/bookings/{token}"))
            .set_json(json!({"starts_at": at + Duration::hours(2)})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(m["starts_at"], json!(at + Duration::hours(2)));
    let (st, m) = send(
        &app,
        test::TestRequest::post().uri(&format!("/public/bookings/{token}/cancel")),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(m["status"], "cancelled");
    let (st, _) = send(
        &app,
        test::TestRequest::post().uri(&format!("/public/bookings/{token}/cancel")),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = send(
        &app,
        test::TestRequest::get().uri("/public/bookings/deadbeefdeadbeefdeadbeefdeadbeef"),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn public_booking_requires_a_verified_phone_when_the_branch_says_so(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_table(&pool, org, branch, "T1", 4).await;
    enable_public(&pool, org, branch, true).await;
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let at = tomorrow_1900();
    let mut body = json!({"branch_id": branch, "starts_at": at, "party_size": 2, "guest_name": "Sara", "phone": "01011111111"});
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/bookings")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    // A token for ANOTHER phone is refused; one for this phone is accepted.
    body["device_token"] =
        json!(crate::delivery::whatsapp::issue_device_token("secret", "201099999999").unwrap());
    let (st, _) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/bookings")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    body["device_token"] =
        json!(crate::delivery::whatsapp::issue_device_token("secret", "201011111111").unwrap());
    let (st, b) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/bookings")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let verified: bool = sqlx::query_scalar("SELECT phone_verified FROM bookings WHERE id = $1")
        .bind(Uuid::parse_str(b["id"].as_str().unwrap()).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(verified);
}

// ── Seating through the ticket, the floor hint, realtime ─────────────────────

#[sqlx::test]
async fn ticket_with_booking_id_seats_the_party_and_settle_completes_it(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    let table = seed_table(&pool, org, branch, "T1", 4).await;
    let item = seed_menu_item(&pool, org, 1000).await;
    seed_cash_method(&pool, org).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let teller = seed_user(&pool, org, "teller").await;
    let shift = open_shift_row(&pool, branch, teller).await;
    let admin_t = token(admin, org, UserRole::OrgAdmin, None);
    let waiter_t = token(waiter, org, UserRole::Waiter, Some(branch));
    let teller_t = token(teller, org, UserRole::Teller, Some(branch));
    let hub = BranchEventHub::new();
    let mut rx = hub.subscribe(branch);
    let app = app!(pool, hub);

    // A booking starting in 10 minutes: inside the hold window right now.
    let at = Utc::now() + Duration::minutes(10);
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &admin_t)
            .set_json(create_body(branch, 3, at)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let booking: BookingView = serde_json::from_value(b).unwrap();
    assert_eq!(booking.table_ids, vec![table]);

    // The realtime bus carried it on the bookings topic.
    let ev = rx.try_recv().expect("booking.created published");
    assert_eq!(ev.topic, Topic::Bookings);
    assert_eq!(ev.event_type, "booking.created");
    assert_eq!(ev.data["id"], json!(booking.id));

    // The floor shows the table's next booking with a held_from in the past.
    let (st, tables) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/floor/tables?branch_id={branch}")),
            &admin_t,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{tables}");
    let hint = &tables[0]["next_booking"];
    assert_eq!(hint["booking_id"], json!(booking.id));
    assert_eq!(hint["guest_name"], "Ahmed");
    let held_from: DateTime<Utc> = serde_json::from_value(hint["held_from"].clone()).unwrap();
    assert!(held_from < Utc::now(), "15-minute hold started");
    assert_eq!(
        tables[0]["status"], "free",
        "bookings never write table status"
    );

    // The waiter fires the party's first round with booking_id.
    let (st, t) = send(
        &app,
        auth(test::TestRequest::post().uri("/open-tickets"), &waiter_t).set_json(json!({
            "branch_id": branch, "table_id": table, "booking_id": booking.id,
            "customer_name": "Ahmed", "guest_count": 3,
            "items": [{ "menu_item_id": item, "quantity": 2 }]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{t}");
    assert_eq!(t["booking_id"], json!(booking.id));
    let ticket_id = t["id"].as_str().unwrap().to_string();
    let (st, b) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/bookings/{}", booking.id)),
            &admin_t,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(b["status"], "seated");
    assert_eq!(b["open_ticket_id"], json!(ticket_id));
    assert!(b["seated_at"].as_str().is_some());

    // The cashier settles → the booking completes.
    let (st, o) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/open-tickets/{ticket_id}/settle")),
            &teller_t,
        )
        .set_json(json!({ "shift_id": shift, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{o}");
    let (_, b) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/bookings/{}", booking.id)),
            &admin_t,
        ),
    )
    .await;
    assert_eq!(b["status"], "completed");

    // Firing against a finished booking is refused.
    let (st, t) = send(
        &app,
        auth(test::TestRequest::post().uri("/open-tickets"), &waiter_t).set_json(json!({
            "branch_id": branch, "booking_id": booking.id,
            "items": [{ "menu_item_id": item, "quantity": 1 }]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{t}");
}

#[sqlx::test]
async fn waiter_seats_and_marks_no_show_including_via_replay(pool: PgPool) {
    perms(&pool).await;
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    seed_table(&pool, org, branch, "T1", 4).await;
    seed_table(&pool, org, branch, "T2", 4).await;
    let admin = seed_user(&pool, org, "org_admin").await;
    let waiter = seed_user(&pool, org, "waiter").await;
    let admin_t = token(admin, org, UserRole::OrgAdmin, None);
    let waiter_t = token(waiter, org, UserRole::Waiter, Some(branch));
    let hub = BranchEventHub::new();
    let app = app!(pool, hub);
    let at = Utc::now() + Duration::minutes(5);

    let (_, b1) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &admin_t)
            .set_json(create_body(branch, 2, at)),
    )
    .await;
    let (_, b2) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &admin_t)
            .set_json(create_body(branch, 2, at)),
    )
    .await;
    let id1 = b1["id"].as_str().unwrap();
    let id2 = b2["id"].as_str().unwrap();

    // Waiter cannot create, may seat.
    let (st, _) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &waiter_t)
            .set_json(create_body(branch, 2, at)),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, b) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/bookings/{id1}/seat")),
            &waiter_t,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "seated");
    // Seating again is idempotent.
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/bookings/{id1}/seat")),
            &waiter_t,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Offline replay of a no-show by the waiter.
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/sync/replay"), &waiter_t)
            .set_json(json!({ "op": "no_show_booking", "teller_id": waiter, "booking_id": id2 })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "no_show");
    // Replaying it again is a clean no-op.
    let (st, _) = send(
        &app,
        auth(test::TestRequest::post().uri("/sync/replay"), &waiter_t)
            .set_json(json!({ "op": "no_show_booking", "teller_id": waiter, "booking_id": id2 })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Stats for the window.
    let from = at - Duration::hours(1);
    let to = at + Duration::hours(1);
    // `Z` suffix: a `+00:00` offset would be decoded as a space in a query string.
    let fmt = |d: DateTime<Utc>| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (st, s) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!(
                "/bookings/stats?branch_id={branch}&from={}&to={}",
                fmt(from),
                fmt(to)
            )),
            &admin_t,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{s}");
    assert_eq!(s["total"], 2);
    assert_eq!(s["seated"], 1);
    assert_eq!(s["no_show"], 1);
    assert_eq!(s["covers"], 2);
    assert!((s["no_show_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9);
}

// ── The sweep ─────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn sweep_reminds_announces_arrivals_and_rolls_no_shows(pool: PgPool) {
    let org = seed_org(&pool).await;
    let branch = seed_branch(&pool, org).await;
    sqlx::query(
        "INSERT INTO branch_booking_settings (branch_id, org_id, reminder_lead_minutes, auto_no_show_minutes, hold_minutes) \
         VALUES ($1, $2, 120, 30, 15)",
    )
    .bind(branch)
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let mk = |start_off: i64, created_off: i64| {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, guest_phone, created_at) \
             VALUES ($1, $2, 2, now() + make_interval(mins => $3), now() + make_interval(mins => $3) + interval '90 minutes', 'A', '2010', now() + make_interval(mins => $4)) RETURNING id",
        )
        .bind(org)
        .bind(branch)
        .bind(start_off as i32)
        .bind(created_off as i32)
    };
    let soon = mk(60, -600).fetch_one(&pool).await.unwrap(); // in 1h, booked 10h ago → reminder + not yet arriving
    let fresh = mk(60, -5).fetch_one(&pool).await.unwrap(); // booked 5 min ago → no reminder
    let due = mk(10, -600).fetch_one(&pool).await.unwrap(); // in 10 min → arriving (hold 15)
    let late = mk(-45, -600).fetch_one(&pool).await.unwrap(); // 45 min ago, grace 30 → no_show
    let far = mk(600, -6000).fetch_one(&pool).await.unwrap(); // in 10h → untouched

    let hub = BranchEventHub::new();
    let mut rx = hub.subscribe(branch);
    super::jobs::run_tick(&pool, &hub).await.unwrap();

    let state = |id: Uuid| {
        sqlx::query_as::<_, (String, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>(
            "SELECT status::text, reminder_sent_at, arriving_notified_at FROM bookings WHERE id = $1",
        )
        .bind(id)
    };
    let s = state(soon).fetch_one(&pool).await.unwrap();
    assert!(s.1.is_some(), "reminder sent");
    assert!(s.2.is_none(), "not arriving yet");
    let s = state(fresh).fetch_one(&pool).await.unwrap();
    assert!(s.1.is_none(), "booked inside the lead → no reminder");
    let s = state(due).fetch_one(&pool).await.unwrap();
    assert!(s.2.is_some(), "arriving announced");
    assert_eq!(s.0, "confirmed");
    let s = state(late).fetch_one(&pool).await.unwrap();
    assert_eq!(s.0, "no_show");
    let s = state(far).fetch_one(&pool).await.unwrap();
    assert_eq!((s.0.as_str(), s.1, s.2), ("confirmed", None, None));

    let mut kinds = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        kinds.push((ev.event_type, ev.data["id"].as_str().map(str::to_string)));
    }
    assert!(
        kinds.contains(&("booking.arriving".into(), Some(due.to_string()))),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("booking.changed".into(), Some(late.to_string()))),
        "{kinds:?}"
    );

    // A second tick is a no-op (idempotent stamps).
    super::jobs::run_tick(&pool, &hub).await.unwrap();
    assert!(rx.try_recv().is_err(), "nothing new to publish");
}
