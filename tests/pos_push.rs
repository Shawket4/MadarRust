//! POS push fallback for a new online order (`push::pos`).
//!
//! The realtime stream is the primary path; FCM rings only a POS device that
//! has no live delivery stream at the order's branch. These tests drive the
//! real routes — public intake, `/push/token`, `/realtime/stream`,
//! `/delivery-orders/{id}/status` — over one shared hub, with FCM replaced by
//! `push::fake`, which records every message the server would have sent.

use std::time::Duration;

use actix_http::Request;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::push::fake;
use madar_rust::realtime::hub::BranchEventHub;

mod common;

const PHONE: &str = "01000000000";
const WAIT: Duration = Duration::from_secs(10);

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}

fn role_of(role: &str) -> UserRole {
    match role {
        "org_admin" => UserRole::OrgAdmin,
        "branch_manager" => UserRole::BranchManager,
        "waiter" => UserRole::Waiter,
        "kitchen" => UserRole::Kitchen,
        _ => UserRole::Teller,
    }
}

// ── the world ─────────────────────────────────────────────────

/// An org with two branches (Arkan, where orders land, and Zamalek), Arkan
/// open for in-mall delivery with a till running, and one item on the menu.
struct World {
    org: Uuid,
    arkan: Uuid,
    zamalek: Uuid,
    item: Uuid,
    hub: BranchEventHub,
}

/// A person with a POS session.
struct Person {
    id: Uuid,
    token: String,
}

async fn world(pool: &PgPool) -> World {
    fake::install();
    madar_rust::push::pos::set_grace(Duration::from_millis(50));
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    let org = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Cafe', $2)")
        .bind(org)
        .bind(format!("org-{org}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES \
         ($1,'cash','{}','e','i',true,true),($1,'card','{}','b','c',false,true)",
    )
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let mut branches = Vec::new();
    for name in ["Arkan", "Zamalek"] {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO branches (id, org_id, name, latitude, longitude) VALUES ($1,$2,$3,30.0,31.0)",
        )
        .bind(id)
        .bind(org)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
        branches.push(id);
    }
    let (arkan, zamalek) = (branches[0], branches[1]);
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, in_mall_enabled, outside_enabled, in_mall_fee) \
         VALUES ($1, true, false, 300)",
    )
    .bind(arkan)
    .execute(pool)
    .await
    .unwrap();
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,'Drinks')")
        .bind(cat)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) \
         VALUES ($1,$2,$3,'Latte',12100,true)",
    )
    .bind(item)
    .bind(org)
    .bind(cat)
    .execute(pool)
    .await
    .unwrap();
    // The branch only takes online orders while a till is open.
    let opener = person(pool, org, "teller", &[arkan]).await;
    sqlx::query(
        "INSERT INTO tills (id, branch_id, teller_id, status, opening_cash) VALUES ($1,$2,$3,'open',10000)",
    )
    .bind(Uuid::new_v4())
    .bind(arkan)
    .bind(opener.id)
    .execute(pool)
    .await
    .unwrap();
    World {
        org,
        arkan,
        zamalek,
        item,
        hub: BranchEventHub::new(),
    }
}

/// A person of `role`, assigned to exactly `branches` (none = org-wide for
/// every role but a branch manager).
async fn person(pool: &PgPool, org: Uuid, role: &str, branches: &[Uuid]) -> Person {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, org_id, name, email, password_hash, role) \
         VALUES ($1,$2,$3,$4,'h',$5::user_role)",
    )
    .bind(id)
    .bind(org)
    .bind(format!("{role} {}", &id.to_string()[..8]))
    .bind(format!("{id}@t.com"))
    .bind(role)
    .execute(pool)
    .await
    .unwrap();
    for b in branches {
        sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1,$2)")
            .bind(id)
            .bind(b)
            .execute(pool)
            .await
            .unwrap();
    }
    let token = create_token(&secret(), id, Some(org), role_of(role), None, 24).unwrap();
    Person { id, token }
}

macro_rules! app {
    ($pool:expr, $w:expr) => {
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new($w.hub.clone()))
                .configure(madar_rust::delivery::routes::configure)
                .configure(madar_rust::push::routes::configure)
                .configure(madar_rust::realtime::routes::configure),
        )
        .await
    };
}

async fn send<S>(app: &S, req: test::TestRequest) -> (StatusCode, Value)
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn auth(req: test::TestRequest, token: &str) -> test::TestRequest {
    req.insert_header(("Authorization", format!("Bearer {token}")))
}

/// `PUT /push/token` from install `device` (the POS sends `X-Madar-Device` on
/// every request). A fresh FCM token each time.
async fn register<S>(
    app: &S,
    who: &Person,
    app_name: &str,
    locale: &str,
    device: Option<Uuid>,
) -> String
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let fcm = format!("fcm-{}", Uuid::new_v4());
    let mut req = auth(test::TestRequest::put().uri("/push/token"), &who.token).set_json(
        json!({ "app": app_name, "token": fcm, "locale": locale, "platform": "android" }),
    );
    if let Some(d) = device {
        req = req.insert_header(("X-Madar-Device", d.to_string()));
    }
    let (st, b) = send(app, req).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{b}");
    fcm
}

/// A POS device: a person signed in on install `device`, registered for pushes.
async fn pos_device<S>(app: &S, who: &Person, locale: &str) -> (Uuid, String)
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let device = Uuid::new_v4();
    let fcm = register(app, who, "pos", locale, Some(device)).await;
    (device, fcm)
}

/// Open `/realtime/stream` from `device` at `branch`. The connection lives as
/// long as the returned response (its body) does.
async fn open_stream<S>(
    app: &S,
    who: &Person,
    branch: Uuid,
    device: Uuid,
    topics: &str,
) -> ServiceResponse
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let req = auth(
        test::TestRequest::get().uri(&format!(
            "/realtime/stream?branch_id={branch}&topics={topics}"
        )),
        &who.token,
    )
    .insert_header(("X-Madar-Device", device.to_string()))
    .to_request();
    let resp = test::call_service(app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    resp
}

fn intake(branch: Uuid, item: Uuid) -> Value {
    let norm = madar_rust::delivery::normalize_phone(PHONE).unwrap();
    let device_token =
        madar_rust::delivery::whatsapp::issue_device_token(&secret().0, &norm).unwrap();
    json!({
        "branch_id": branch, "channel": "in_mall",
        "customer_name": "Sara", "customer_phone": PHONE,
        "place_name": "Shop 12", "floor": "2", "unit_number": "B4",
        "customer_lat": 30.0, "customer_lng": 31.0,
        "payment_method_hint": "cash", "device_token": device_token,
        "items": [{ "menu_item_id": item, "quantity": 2 }],
    })
}

/// Place an online order at Arkan and wait for its push fan-out to finish.
async fn place<S>(app: &S, w: &World) -> Value
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    place_with(app, w, None).await
}

async fn place_with<S>(app: &S, w: &World, idem: Option<Uuid>) -> Value
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let mut req = test::TestRequest::post()
        .uri("/public/delivery-orders")
        .set_json(intake(w.arkan, w.item));
    if let Some(k) = idem {
        req = req.insert_header(("Idempotency-Key", k.to_string()));
    }
    let (st, order) = send(app, req).await;
    assert!(
        st == StatusCode::CREATED || st == StatusCode::OK,
        "intake failed: {st} {order}"
    );
    order
}

fn order_tag(order: &Value) -> String {
    format!("delivery.created:{}", order["id"].as_str().unwrap())
}

async fn settled(order: &Value) {
    assert!(
        fake::wait_done(&order_tag(order), WAIT).await,
        "the push fan-out never finished"
    );
}

async fn live(pool: &PgPool, fcm: &str) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM push_devices WHERE token = $1 AND revoked_at IS NULL)",
    )
    .bind(fcm)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── who gets it ───────────────────────────────────────────────

#[sqlx::test]
async fn a_new_order_rings_the_tellers_and_managers_at_its_branch(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let manager = person(&pool, w.org, "branch_manager", &[w.arkan]).await;
    let owner = person(&pool, w.org, "org_admin", &[]).await;
    let (_, t_fcm) = pos_device(&app, &teller, "en").await;
    let (_, m_fcm) = pos_device(&app, &manager, "ar").await;
    let (_, o_fcm) = pos_device(&app, &owner, "en").await;

    let order = place(&app, &w).await;
    settled(&order).await;

    for fcm in [&t_fcm, &m_fcm, &o_fcm] {
        assert_eq!(fake::sent_to(fcm).len(), 1, "exactly one push to {fcm}");
    }
}

#[sqlx::test]
async fn staff_of_another_branch_are_not_rung(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let z_teller = person(&pool, w.org, "teller", &[w.zamalek]).await;
    let z_manager = person(&pool, w.org, "branch_manager", &[w.zamalek]).await;
    // A manager with no branch at all works nowhere.
    let unassigned_manager = person(&pool, w.org, "branch_manager", &[]).await;
    let a_teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, zt) = pos_device(&app, &z_teller, "en").await;
    let (_, zm) = pos_device(&app, &z_manager, "en").await;
    let (_, um) = pos_device(&app, &unassigned_manager, "en").await;
    let (_, at) = pos_device(&app, &a_teller, "en").await;

    let order = place(&app, &w).await;
    settled(&order).await;

    assert!(fake::sent_to(&zt).is_empty(), "Zamalek teller");
    assert!(fake::sent_to(&zm).is_empty(), "Zamalek manager");
    assert!(fake::sent_to(&um).is_empty(), "a manager assigned nowhere");
    assert_eq!(fake::sent_to(&at).len(), 1, "the Arkan teller still is");
}

#[sqlx::test]
async fn nobody_without_the_accept_capability_at_the_branch_is_rung(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let waiter = person(&pool, w.org, "waiter", &[w.arkan]).await;
    let kitchen = person(&pool, w.org, "kitchen", &[w.arkan]).await;
    // A teller the owner took "accept delivery orders" away from, at Arkan only.
    let denied = person(&pool, w.org, "teller", &[w.arkan, w.zamalek]).await;
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect, branch_id) \
         VALUES ($1, $2, $3, 'deny', $4)",
    )
    .bind(w.org)
    .bind(denied.id)
    .bind(madar_rust::push::pos::CAP.id() as i16)
    .bind(w.arkan)
    .execute(&pool)
    .await
    .unwrap();
    let (_, wf) = pos_device(&app, &waiter, "en").await;
    let (_, kf) = pos_device(&app, &kitchen, "en").await;
    let (_, df) = pos_device(&app, &denied, "en").await;

    let order = place(&app, &w).await;
    settled(&order).await;

    assert!(fake::sent_to(&wf).is_empty(), "waiter");
    assert!(fake::sent_to(&kf).is_empty(), "kitchen");
    assert!(fake::sent_to(&df).is_empty(), "teller denied at Arkan");
}

#[sqlx::test]
async fn a_waiter_given_the_capability_is_still_not_rung(pool: PgPool) {
    // The push follows the live banner, and a waiter's POS never alerts on a
    // new delivery (the core's `role_wants_alert`), whatever they may do.
    let w = world(&pool).await;
    let app = app!(pool, w);
    let waiter = person(&pool, w.org, "waiter", &[w.arkan]).await;
    sqlx::query(
        "INSERT INTO user_overrides (org_id, user_id, capability_id, effect) \
         VALUES ($1, $2, $3, 'allow')",
    )
    .bind(w.org)
    .bind(waiter.id)
    .bind(madar_rust::push::pos::CAP.id() as i16)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        madar_rust::authz::require::effective(&pool, waiter.id, Some(w.arkan))
            .await
            .unwrap()
            .can(madar_rust::push::pos::CAP),
        "the override took"
    );
    let (_, fcm) = pos_device(&app, &waiter, "en").await;

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(fake::sent_to(&fcm).is_empty());
}

#[sqlx::test]
async fn only_pos_devices_are_rung_not_dawam_or_other_apps(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, pos) = pos_device(&app, &teller, "en").await;
    let manager_app = register(&app, &teller, "manager", "en", Some(Uuid::new_v4())).await;
    // A Dawam staff-app phone, registered for the employee (never via /push/token).
    let e = common::employees::employee(&pool, w.org, "E", None, None, false, &[], 0).await;
    sqlx::query(
        "INSERT INTO push_devices (org_id, employee_id, app, token, locale) \
         VALUES ($1, $2, 'dawam', 'dawam-phone', 'ar')",
    )
    .bind(w.org)
    .bind(e)
    .execute(&pool)
    .await
    .unwrap();

    let order = place(&app, &w).await;
    settled(&order).await;

    assert_eq!(fake::sent_to(&pos).len(), 1);
    assert!(fake::sent_to(&manager_app).is_empty(), "another app");
    assert!(fake::sent_to("dawam-phone").is_empty(), "Dawam");
}

#[sqlx::test]
async fn a_signed_out_till_is_not_rung(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, fcm) = pos_device(&app, &teller, "en").await;
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::delete().uri("/push/token"),
            &teller.token,
        )
        .set_json(json!({ "app": "pos", "token": fcm })),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(fake::sent_to(&fcm).is_empty());
}

#[sqlx::test]
async fn a_deactivated_person_is_not_rung(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, fcm) = pos_device(&app, &teller, "en").await;
    sqlx::query("UPDATE users SET is_active = false WHERE id = $1")
        .bind(teller.id)
        .execute(&pool)
        .await
        .unwrap();

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(fake::sent_to(&fcm).is_empty());
}

// ── what it says ──────────────────────────────────────────────

#[sqlx::test]
async fn the_words_follow_the_devices_language(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let en_teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let ar_teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, en) = pos_device(&app, &en_teller, "en").await;
    let (_, ar) = pos_device(&app, &ar_teller, "ar").await;

    let order = place(&app, &w).await;
    settled(&order).await;

    let reference = order["delivery_ref"].as_str().unwrap();
    let total = order["total"].as_i64().unwrap();
    let money = format!("{}.{:02} EGP", total / 100, total % 100);
    let en_msg = &fake::sent_to(&en)[0]["message"];
    assert_eq!(en_msg["notification"]["title"], "New delivery order");
    assert_eq!(
        en_msg["notification"]["body"],
        format!("{reference} · In-Mall · {money}")
    );
    let ar_msg = &fake::sent_to(&ar)[0]["message"];
    assert_eq!(ar_msg["notification"]["title"], "طلب توصيل جديد");
    assert_eq!(
        ar_msg["notification"]["body"],
        format!("{reference} · داخل المول · {money}")
    );
}

#[sqlx::test]
async fn the_payload_carries_the_order_and_the_live_banners_tag(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, fcm) = pos_device(&app, &teller, "en").await;

    let order = place(&app, &w).await;
    settled(&order).await;

    let id = order["id"].as_str().unwrap();
    let m = &fake::sent_to(&fcm)[0]["message"];
    assert_eq!(m["token"], fcm.as_str());
    assert_eq!(m["data"]["order_id"], id);
    assert_eq!(m["data"]["tag"], format!("delivery.created:{id}"));
    assert_eq!(m["data"]["kind"], "online_order");
    assert_eq!(m["data"]["title"], m["notification"]["title"]);
    assert_eq!(m["data"]["body"], m["notification"]["body"]);
    // FCM data values must all be strings.
    for (k, v) in m["data"].as_object().unwrap() {
        assert!(v.is_string(), "data.{k} is not a string");
    }
    assert_eq!(m["android"]["priority"], "high");
    assert_eq!(m["android"]["notification"]["channel_id"], "madar_realtime");
    assert_eq!(m["android"]["notification"]["tag"], m["data"]["tag"]);
    assert_eq!(m["apns"]["headers"]["apns-priority"], "10");
    assert_eq!(m["apns"]["payload"]["aps"]["sound"], "default");
    assert_eq!(m["apns"]["headers"]["apns-collapse-id"], m["data"]["tag"]);
}

// ── once, and never in the way ────────────────────────────────

#[sqlx::test]
async fn one_push_per_order_not_on_a_replay_or_a_status_change(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, fcm) = pos_device(&app, &teller, "en").await;

    let key = Uuid::new_v4();
    let order = place_with(&app, &w, Some(key)).await;
    settled(&order).await;
    // The customer's app retries the same order.
    let again = place_with(&app, &w, Some(key)).await;
    assert_eq!(again["id"], order["id"], "an idempotent replay");
    // The counter accepts it.
    let (st, b) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!(
                "/delivery-orders/{}/status",
                order["id"].as_str().unwrap()
            )),
            &teller.token,
        )
        .set_json(json!({ "status": "confirmed" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(fake::done_count(&order_tag(&order)), 1, "one fan-out");
    assert_eq!(fake::sent_to(&fcm).len(), 1, "one push");
    // A second order is its own push.
    let second = place(&app, &w).await;
    settled(&second).await;
    assert_eq!(fake::sent_to(&fcm).len(), 2);
}

#[sqlx::test]
async fn a_refreshed_token_replaces_the_old_one_on_the_same_till(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let device = Uuid::new_v4();
    let old = register(&app, &teller, "pos", "en", Some(device)).await;
    let new = register(&app, &teller, "pos", "en", Some(device)).await;
    assert!(!live(&pool, &old).await, "the old token is revoked");

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(fake::sent_to(&old).is_empty());
    assert_eq!(fake::sent_to(&new).len(), 1, "the till rings once");
}

#[sqlx::test]
async fn the_order_is_taken_even_when_fcm_is_down(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, fcm) = pos_device(&app, &teller, "en").await;
    fake::respond_all(503);

    let (st, order) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .set_json(intake(w.arkan, w.item)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{order}");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM delivery_orders WHERE id = $1")
        .bind(Uuid::parse_str(order["id"].as_str().unwrap()).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
    settled(&order).await;
    assert_eq!(fake::sent_to(&fcm).len(), 2, "tried, retried once, gave up");
    assert!(
        live(&pool, &fcm).await,
        "an outage never unregisters a till"
    );
}

#[sqlx::test]
async fn a_token_fcm_says_is_gone_is_dropped_and_a_bad_message_drops_nothing(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let a = person(&pool, w.org, "teller", &[w.arkan]).await;
    let b = person(&pool, w.org, "teller", &[w.arkan]).await;
    let c = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (_, gone) = pos_device(&app, &a, "en").await;
    let (_, bad_token) = pos_device(&app, &b, "en").await;
    let (_, bad_message) = pos_device(&app, &c, "en").await;
    fake::respond(
        &gone,
        404,
        json!({"error": {"code": 404, "status": "NOT_FOUND",
            "details": [{"@type": "type.googleapis.com/google.firebase.fcm.v1.FcmError",
                         "errorCode": "UNREGISTERED"}]}}),
    );
    fake::respond(
        &bad_token,
        400,
        json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "details": [
            {"@type": "type.googleapis.com/google.rpc.BadRequest",
             "fieldViolations": [{"field": "message.token"}]}]}}),
    );
    fake::respond(
        &bad_message,
        400,
        json!({"error": {"code": 400, "status": "INVALID_ARGUMENT", "details": [
            {"@type": "type.googleapis.com/google.rpc.BadRequest",
             "fieldViolations": [{"field": "message.apns.headers"}]}]}}),
    );

    let order = place(&app, &w).await;
    settled(&order).await;

    assert!(!live(&pool, &gone).await, "UNREGISTERED drops the token");
    assert!(
        !live(&pool, &bad_token).await,
        "a 400 naming the token drops it"
    );
    assert!(
        live(&pool, &bad_message).await,
        "a 400 about the message keeps it"
    );
}

// ── SSE first, FCM only as the fallback ───────────────────────

#[sqlx::test]
async fn a_till_watching_the_live_stream_gets_no_push(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (device, fcm) = pos_device(&app, &teller, "en").await;
    let stream = open_stream(&app, &teller, w.arkan, device, "delivery,orders").await;
    assert!(w.hub.is_connected(w.arkan, device));

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(
        fake::sent_to(&fcm).is_empty(),
        "the open app alerts on its own"
    );
    drop(stream);
}

#[sqlx::test]
async fn a_till_with_no_live_stream_is_pushed(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (device, fcm) = pos_device(&app, &teller, "en").await;
    assert!(!w.hub.is_connected(w.arkan, device));

    let order = place(&app, &w).await;
    settled(&order).await;
    assert_eq!(fake::sent_to(&fcm).len(), 1);
}

#[sqlx::test]
async fn a_till_whose_stream_dropped_is_pushed(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (device, fcm) = pos_device(&app, &teller, "en").await;
    let stream = open_stream(&app, &teller, w.arkan, device, "delivery").await;
    assert!(w.hub.is_connected(w.arkan, device));
    drop(stream); // the app went to the background / lost its network
    assert!(!w.hub.is_connected(w.arkan, device));

    let order = place(&app, &w).await;
    settled(&order).await;
    assert_eq!(fake::sent_to(&fcm).len(), 1);
}

#[sqlx::test]
async fn of_two_tills_only_the_one_not_watching_is_pushed(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let front = person(&pool, w.org, "teller", &[w.arkan]).await;
    let back = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (front_device, front_fcm) = pos_device(&app, &front, "en").await;
    let (_, back_fcm) = pos_device(&app, &back, "en").await;
    let _stream = open_stream(&app, &front, w.arkan, front_device, "delivery").await;

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(fake::sent_to(&front_fcm).is_empty(), "watching");
    assert_eq!(fake::sent_to(&back_fcm).len(), 1, "closed");
}

#[sqlx::test]
async fn one_person_on_two_tills_is_pushed_only_on_the_closed_one(pool: PgPool) {
    // Matching is per INSTALL, not per person: the same manager signed in on
    // an open till and a closed one rings only the closed one.
    let w = world(&pool).await;
    let app = app!(pool, w);
    let manager = person(&pool, w.org, "branch_manager", &[w.arkan]).await;
    let (open_device, open_fcm) = pos_device(&app, &manager, "en").await;
    let (_, closed_fcm) = pos_device(&app, &manager, "en").await;
    let _stream = open_stream(&app, &manager, w.arkan, open_device, "delivery").await;

    let order = place(&app, &w).await;
    settled(&order).await;
    assert!(fake::sent_to(&open_fcm).is_empty());
    assert_eq!(fake::sent_to(&closed_fcm).len(), 1);
}

#[sqlx::test]
async fn a_stream_at_another_branch_or_without_delivery_does_not_count(pool: PgPool) {
    let w = world(&pool).await;
    let app = app!(pool, w);
    let manager = person(&pool, w.org, "branch_manager", &[w.arkan, w.zamalek]).await;
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (m_device, m_fcm) = pos_device(&app, &manager, "en").await;
    let (t_device, t_fcm) = pos_device(&app, &teller, "en").await;
    // The manager's till watches Zamalek; the teller's stream carries no orders.
    let _a = open_stream(&app, &manager, w.zamalek, m_device, "delivery").await;
    let _b = open_stream(&app, &teller, w.arkan, t_device, "kitchen").await;
    assert!(!w.hub.is_connected(w.arkan, t_device));

    let order = place(&app, &w).await;
    settled(&order).await;
    assert_eq!(
        fake::sent_to(&m_fcm).len(),
        1,
        "Zamalek's stream never hears Arkan"
    );
    assert_eq!(
        fake::sent_to(&t_fcm).len(),
        1,
        "no delivery topic, no live banner"
    );
}

#[sqlx::test]
async fn a_till_that_reconnects_within_the_grace_window_is_not_pushed(pool: PgPool) {
    let w = world(&pool).await;
    madar_rust::push::pos::set_grace(Duration::from_millis(1500));
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let (device, fcm) = pos_device(&app, &teller, "en").await;

    let order = place(&app, &w).await;
    // Back online a moment later: it replays `delivery.created` itself.
    let _stream = open_stream(&app, &teller, w.arkan, device, "delivery").await;
    settled(&order).await;
    assert!(fake::sent_to(&fcm).is_empty());
}

#[sqlx::test]
async fn a_registration_that_names_no_install_is_always_pushed(pool: PgPool) {
    // An older build that sends no X-Madar-Device cannot be matched to a
    // stream, so it is never assumed to be watching.
    let w = world(&pool).await;
    let app = app!(pool, w);
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let fcm = register(&app, &teller, "pos", "en", None).await;
    let _stream = open_stream(&app, &teller, w.arkan, Uuid::new_v4(), "delivery").await;

    let order = place(&app, &w).await;
    settled(&order).await;
    assert_eq!(fake::sent_to(&fcm).len(), 1);
}

/// Over a real socket: a client that closes its connection is deregistered at
/// once (the server's `h1_allow_half_closed(false)`), not a keep-alive ping or
/// two later.
#[sqlx::test]
async fn a_closed_socket_releases_the_till_at_once(pool: PgPool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let w = world(&pool).await;
    let teller = person(&pool, w.org, "teller", &[w.arkan]).await;
    let device = Uuid::new_v4();
    let hub = w.hub.clone();
    let opts = (*pool.connect_options()).clone();
    let server = actix_web::HttpServer::new(move || {
        // A pool per worker runtime (connections are bound to the runtime
        // that opened them).
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(opts.clone());
        App::new()
            .app_data(web::Data::new(pool))
            .app_data(web::Data::new(secret()))
            .app_data(web::Data::new(hub.clone()))
            .configure(madar_rust::realtime::routes::configure)
    })
    .h1_allow_half_closed(madar_rust::realtime::H1_ALLOW_HALF_CLOSED)
    .workers(1)
    .disable_signals()
    .bind(("127.0.0.1", 0))
    .unwrap();
    let addr = server.addrs()[0];
    let server = server.run();
    let handle = server.handle();
    tokio::spawn(server);

    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /realtime/stream?branch_id={}&topics=delivery HTTP/1.1\r\nHost: x\r\n\
         Authorization: Bearer {}\r\nX-Madar-Device: {device}\r\nAccept: text/event-stream\r\n\r\n",
        w.arkan, teller.token
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = [0u8; 512];
    let n = tokio::time::timeout(WAIT, sock.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&buf[..n])
    );
    assert!(w.hub.is_connected(w.arkan, device));

    drop(sock);
    let start = std::time::Instant::now();
    while w.hub.is_connected(w.arkan, device) && start.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !w.hub.is_connected(w.arkan, device),
        "still 'connected' {:?} after the socket closed",
        start.elapsed()
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "released after {:?}",
        start.elapsed()
    );
    handle.stop(false).await;
}
