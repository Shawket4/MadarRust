//! The customers unification, wave 2 (CUSTOMERS_UNIFICATION_DESIGN.md §2.4–2.8,
//! §4): every entry point links the customer and never renames them, addresses
//! are deduplicated on write, erase leaves no personal data anywhere, leaving
//! the programme keeps the person, a retired Apple pass is served voided, and
//! "order now" keeps its trust model — the token identifies, the device
//! authorises.

use std::borrow::Cow;

use actix_http::Request;
use actix_web::dev::{Service, ServiceResponse};
use actix_web::{App, http::StatusCode, test, web};
use madar_rust::auth::jwt::{JwtSecret, create_token};
use madar_rust::models::UserRole;
use madar_rust::realtime::hub::BranchEventHub;
use serde_json::{Value, json};
use sqlx::PgPool;
use sqlx::migrate::Migrator;
use uuid::Uuid;

mod common;
use common::members::seed_loyalty_member;

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");
/// The first wave-2 migration.
const REFERENCES: i64 = 20260925070000;

fn secret() -> JwtSecret {
    JwtSecret("secret".into())
}
fn admin_token(uid: Uuid, org: Uuid) -> String {
    create_token(&secret(), uid, Some(org), UserRole::OrgAdmin, None, 24).unwrap()
}
fn teller_token(uid: Uuid, org: Uuid, branch: Uuid) -> String {
    create_token(
        &secret(),
        uid,
        Some(org),
        UserRole::Teller,
        Some(branch),
        24,
    )
    .unwrap()
}
fn device_token(raw_phone: &str) -> String {
    let norm = madar_rust::phone::normalize_phone(raw_phone).unwrap();
    madar_rust::delivery::whatsapp::issue_device_token(&secret().0, &norm).unwrap()
}
fn u(s: &str) -> Uuid {
    Uuid::parse_str(s).unwrap()
}

macro_rules! app {
    ($pool:expr) => {{
        // These tests drive the identity endpoints more than five times from
        // one address; the per-IP limiter is not what they are about. Read once,
        // when the routes are configured. (nextest: one process per test.)
        // SAFETY: set before any other thread of this test exists to read it.
        unsafe { std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1") };
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::customers::routes::configure)
                .configure(madar_rust::loyalty::routes::configure)
                .configure(madar_rust::loyalty::wallet::web_service::configure)
                .configure(madar_rust::delivery::routes::configure)
                .configure(madar_rust::bookings::routes::configure)
                .configure(madar_rust::tickets::routes::configure)
                .configure(madar_rust::kitchen::routes::configure)
                .configure(madar_rust::orders::routes::configure)
                .configure(madar_rust::reservations::routes::configure)
                .configure(madar_rust::sync::routes::configure),
        )
        .await
    }};
}

/// The same routes WITH their limiters: never sets the disable flag, so it is
/// only for a test that is about the limiter. (nextest: one process per test.)
macro_rules! limited_app {
    ($pool:expr) => {{
        assert!(
            std::env::var("MADAR_DISABLE_RATE_LIMIT").is_err(),
            "the limiter tests need the limiter: do not export MADAR_DISABLE_RATE_LIMIT"
        );
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(BranchEventHub::new()))
                .configure(madar_rust::customers::routes::configure),
        )
        .await
    }};
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

// ── seeds ───────────────────────────────────────────────────────────────────

async fn seed_org(pool: &PgPool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO organizations (id, name, slug) VALUES ($1, 'Org', $2)")
        .bind(id)
        .bind(format!("org-{id}"))
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO org_payment_methods (org_id, name, label_translations, color, icon, is_cash, is_active) VALUES \
         ($1,'cash','{}','e','i',true,true),($1,'card','{}','b','c',false,true)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn seed_branch(pool: &PgPool, org: Uuid, name: &str) -> Uuid {
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
    id
}
async fn seed_user(pool: &PgPool, org: Uuid, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ($1,$2,$5,$3,'h',$4::user_role)")
        .bind(id)
        .bind(org)
        .bind(format!("u-{id}@t.com"))
        .bind(role)
        .bind(format!("U {}", &id.to_string()[..8]))
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn open_till(pool: &PgPool, branch: Uuid, user: Uuid) -> Uuid {
    sqlx::query_scalar("INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1,$2,'open',0) RETURNING id")
        .bind(branch)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
}
/// In-mall and outside both on, no OTP unless asked, one 50 km ring.
async fn seed_ordering(pool: &PgPool, branch: Uuid, otp_required: bool) {
    sqlx::query(
        "INSERT INTO branch_delivery_settings (branch_id, in_mall_enabled, outside_enabled, in_mall_fee, otp_required) \
         VALUES ($1, true, true, 0, $2)",
    )
    .bind(branch)
    .bind(otp_required)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO delivery_zones (branch_id, name, max_road_distance_meters, fee) VALUES ($1,'Zone 1',50000,2500)")
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
}
async fn seed_item(pool: &PgPool, org: Uuid, price: i32) -> Uuid {
    let cat = Uuid::new_v4();
    sqlx::query("INSERT INTO categories (id, org_id, name) VALUES ($1,$2,$3)")
        .bind(cat)
        .bind(org)
        .bind(format!("C-{cat}"))
        .execute(pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO menu_items (id, org_id, category_id, name, base_price, is_active) VALUES ($1,$2,$3,'Item',$4,true)")
        .bind(id)
        .bind(org)
        .bind(cat)
        .bind(price)
        .execute(pool)
        .await
        .unwrap();
    id
}
async fn perms(pool: &PgPool) {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
}
async fn i64_of(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// A shop that takes online orders: org, branch (open till), one menu item.
struct Shop {
    org: Uuid,
    branch: Uuid,
    teller: Uuid,
    till: Uuid,
    admin: Uuid,
    item: Uuid,
}
async fn shop(pool: &PgPool, otp_required: bool) -> Shop {
    perms(pool).await;
    let org = seed_org(pool).await;
    let branch = seed_branch(pool, org, "Maadi").await;
    let teller = seed_user(pool, org, "teller").await;
    let admin = seed_user(pool, org, "org_admin").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1,$2)")
        .bind(teller)
        .bind(branch)
        .execute(pool)
        .await
        .unwrap();
    let till = open_till(pool, branch, teller).await;
    seed_ordering(pool, branch, otp_required).await;
    let item = seed_item(pool, org, 5000).await;
    Shop {
        org,
        branch,
        teller,
        till,
        admin,
        item,
    }
}

const SARA: &str = "01000000000";
const SARA_KEY: &str = "201000000000";

fn outside_order(s: &Shop, name: &str, phone: &str, address: &str, lat: f64, lng: f64) -> Value {
    json!({
        "branch_id": s.branch, "channel": "outside",
        "customer_name": name, "customer_phone": phone,
        "address_line": address, "unit_number": "4B",
        "customer_lat": lat, "customer_lng": lng,
        "payment_method_hint": "card", "device_token": device_token(phone),
        "items": [{ "menu_item_id": s.item, "quantity": 1 }],
    })
}
async fn place<S>(app: &S, body: &Value) -> (StatusCode, Value)
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    send(
        app,
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .set_json(body),
    )
    .await
}
async fn customer_of_phone(pool: &PgPool, org: Uuid, key: &str) -> Option<(Uuid, String, String)> {
    sqlx::query_as(
        "SELECT id, name, source FROM customers WHERE org_id = $1 AND phone_key = $2
            AND merged_into IS NULL AND erased_at IS NULL",
    )
    .bind(org)
    .bind(key)
    .fetch_optional(pool)
    .await
    .unwrap()
}

// ── B: entry points ─────────────────────────────────────────────────────────

/// An online order makes the customer on first contact, links to them, and the
/// sale it becomes carries the same id — on the REST order too.
#[sqlx::test]
async fn a_delivery_order_links_its_customer_and_so_does_its_sale(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara", SARA, "12 Tahrir St", 30.001, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    let (cid, name, source) = customer_of_phone(&pool, s.org, SARA_KEY)
        .await
        .expect("customer created");
    assert_eq!((name.as_str(), source.as_str()), ("Sara", "online"));
    assert_eq!(
        d["customer_id"],
        json!(cid),
        "the delivery order response names the customer"
    );
    assert_eq!(d["contact_override"], json!(false));
    assert!(d["address_id"].is_string(), "the address was saved: {d}");

    // The same person again, typing a different name: matched, NOT renamed.
    let (st, d2) = place(
        &app,
        &outside_order(
            &s,
            "Sara Mostafa",
            "+20 100 000 0000",
            "12 Tahrir St",
            30.001,
            31.001,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d2}");
    assert_eq!(d2["customer_id"], json!(cid));
    assert_eq!(
        d2["customer_name"], "Sara Mostafa",
        "the snapshot is what was typed"
    );
    assert_eq!(
        customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap().1,
        "Sara",
        "the stored name never moves"
    );
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM customers").await, 1);

    // Finalize → the sale belongs to the same customer, and says so over REST.
    let id = d["id"].as_str().unwrap();
    let (st, f) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/delivery-orders/{id}/finalize")),
            &teller_token(s.teller, s.org, s.branch),
        )
        .set_json(json!({ "shift_id": s.till, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "finalize: {f}");
    let order_id = f["order_id"].as_str().unwrap();
    let on_row: Option<Uuid> = sqlx::query_scalar("SELECT customer_id FROM orders WHERE id = $1")
        .bind(u(order_id))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(on_row, Some(cid));
    let (st, o) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/orders/{order_id}")),
            &admin_token(s.admin, s.org),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{o}");
    let got = o
        .get("customer_id")
        .or_else(|| o["order"].get("customer_id"))
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(
        got,
        json!(cid),
        "GET /orders/{{id}} carries customer_id: {o}"
    );
}

/// Two first orders from one new phone at the same moment: one customer.
#[sqlx::test]
async fn two_first_orders_from_one_new_phone_make_one_customer(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let a = outside_order(&s, "Sara", SARA, "1 Nile St", 30.001, 31.001);
    let b = outside_order(&s, "Sara", SARA, "2 Nile St", 30.002, 31.002);
    let (ra, rb) = futures::join!(place(&app, &a), place(&app, &b));
    assert_eq!(ra.0, StatusCode::CREATED, "{}", ra.1);
    assert_eq!(rb.0, StatusCode::CREATED, "{}", rb.1);
    assert_eq!(ra.1["customer_id"], rb.1["customer_id"]);
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM customers").await, 1);
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM customer_addresses").await,
        2
    );
}

/// A host booking and a public-style booking link the customer; the stored
/// name is never overwritten.
#[sqlx::test]
async fn a_booking_links_its_customer_and_never_renames_them(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let tok = admin_token(s.admin, s.org);
    let at = chrono::Utc::now() + chrono::Duration::days(1);
    let body = |name: &str| {
        json!({
            "branch_id": s.branch, "party_size": 2, "starts_at": at, "guest_name": name,
            "guest_phone": "0100 000 0000", "force": true, "table_ids": [], "send_confirmation": false
        })
    };
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(body("Sara")),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let (cid, _, source) = customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap();
    assert_eq!(source, "booking");
    assert_eq!(
        b["customer_id"],
        json!(cid),
        "the booking response names the customer: {b}"
    );

    let at2 = at + chrono::Duration::hours(3);
    let mut again = body("S. Mostafa");
    again["starts_at"] = json!(at2);
    let (st, b2) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(again),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b2}");
    assert_eq!(b2["customer_id"], json!(cid));
    assert_eq!(b2["guest_name"], "S. Mostafa");
    assert_eq!(
        customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap().1,
        "Sara"
    );
}

/// A table-QR guest who gives a phone is linked; one who gives a bad phone (or
/// none) still orders. Settling carries the bill's customer onto the sale.
#[sqlx::test]
async fn a_table_order_with_a_phone_links_the_bill_and_the_sale(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let table = Uuid::new_v4();
    let other = Uuid::new_v4();
    for (id, label) in [(table, "T1"), (other, "T2")] {
        sqlx::query(
            "INSERT INTO branch_tables (id, org_id, branch_id, label) VALUES ($1,$2,$3,$4)",
        )
        .bind(id)
        .bind(s.org)
        .bind(s.branch)
        .bind(label)
        .execute(&pool)
        .await
        .unwrap();
    }
    let order = |t: Uuid, phone: Value| {
        json!({
            "table_id": t, "customer_name": "Sara", "customer_phone": phone,
            "idempotency_key": Uuid::new_v4(), "items": [{ "menu_item_id": s.item, "quantity": 1 }]
        })
    };
    let (st, v) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/table-orders")
            .set_json(order(table, json!(SARA))),
    )
    .await;
    assert!(st.is_success(), "{v}");
    let ticket = u(v["id"].as_str().unwrap());
    let (cid, _, source) = customer_of_phone(&pool, s.org, SARA_KEY)
        .await
        .expect("customer");
    assert_eq!(source, "table_qr");
    let linked: Option<Uuid> =
        sqlx::query_scalar("SELECT customer_id FROM open_tickets WHERE id = $1")
            .bind(ticket)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(linked, Some(cid));

    // A phone that is not a phone: the order goes through, unlinked.
    let (st, v2) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/table-orders")
            .set_json(order(other, json!("12345"))),
    )
    .await;
    assert!(st.is_success(), "a bad phone never refuses the order: {v2}");
    assert!(v2["customer_id"].is_null());
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM customers").await, 1);

    // The ticket view (REST and therefore the sync projection) names them.
    let tok = teller_token(s.teller, s.org, s.branch);
    let (st, view) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/open-tickets/{ticket}")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{view}");
    assert_eq!(view["customer_id"], json!(cid));

    let (st, o) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/open-tickets/{ticket}/settle")),
            &tok,
        )
        .set_json(json!({ "shift_id": s.till, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "settle: {o}");
    assert_eq!(
        o["customer_id"],
        json!(cid),
        "the settled sale belongs to the bill's customer: {o}"
    );
}

/// The source-text guard (design §3.7): a customer row is written in ONE place.
/// `INSERT INTO customers` anywhere in `src/` outside `src/customers/` is a new
/// way for an unlinked or duplicate person to appear.
#[core::prelude::v1::test]
fn customers_are_inserted_only_by_the_customers_module() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let root = std::env::var("CARGO_MANIFEST_DIR")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string());
    let src = std::path::Path::new(&root).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    assert!(
        files.len() > 50,
        "the guard must actually be reading src/ ({} files at {src:?})",
        files.len()
    );
    let needle = regex::Regex::new(r"(?i)insert\s+into\s+(public\.)?customers\b").unwrap();
    let offenders: Vec<String> = files
        .iter()
        .filter(|p| !p.strip_prefix(&src).unwrap().starts_with("customers"))
        // Production code only: an inline `#[cfg(test)]` block may seed a row
        // directly, exactly as the integration suites do.
        .filter(|p| {
            let text = std::fs::read_to_string(p).unwrap();
            needle.is_match(text.split("#[cfg(test)]").next().unwrap_or(""))
        })
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "customers are created only through customers::resolve_or_create / insert_customer; found an INSERT in: {offenders:?}"
    );
    // And the guard can see the one that is allowed.
    assert!(needle.is_match(&std::fs::read_to_string(src.join("customers/handlers.rs")).unwrap()));
}

// ── C: addresses ────────────────────────────────────────────────────────────

#[sqlx::test]
async fn addresses_are_deduplicated_on_write_and_gated_for_staff(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    // Same text (modulo case and punctuation) → one row, used twice.
    for addr in ["12 Tahrir St., Flat 4", "12 tahrir st flat 4"] {
        let (st, d) = place(&app, &outside_order(&s, "Sara", SARA, addr, 30.001, 31.001)).await;
        assert_eq!(st, StatusCode::CREATED, "{d}");
    }
    // Different words, same unit, 11 m away → still the same place.
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara", SARA, "Tahrir street twelve", 30.0011, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    // Somewhere else entirely.
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara", SARA, "9 Road 9, Maadi", 30.02, 31.02),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    // An abandoned cart saves nothing: a refused order leaves no address.
    let mut bad = outside_order(&s, "Sara", SARA, "77 Nowhere", 10.0, 10.0);
    bad["payment_method_hint"] = json!("cash");
    let (st, _) = place(&app, &bad).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "out of range");

    let rows: Vec<(i32,)> =
        sqlx::query_as("SELECT use_count FROM customer_addresses ORDER BY use_count DESC")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows, vec![(3,), (1,)]);

    let (cid, ..) = customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap();
    let (st, list) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/customers/{cid}/addresses")),
            &admin_token(s.admin, s.org),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{list}");
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(
        list[0]["address_line"], "9 Road 9, Maadi",
        "most recently used first"
    );

    // A waiter does not hold customers.addresses.view.
    let waiter = seed_user(&pool, s.org, "waiter").await;
    let wtok = create_token(
        &secret(),
        waiter,
        Some(s.org),
        UserRole::Waiter,
        Some(s.branch),
        24,
    )
    .unwrap();
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/customers/{cid}/addresses")),
            &wtok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // The public "past locations" reads the same rows, behind the device token.
    let (st, locs) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/delivery-orders/past-locations?phone={SARA}&org_id={}&device_token={}",
            s.org,
            device_token(SARA)
        )),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{locs}");
    assert_eq!(locs.as_array().unwrap().len(), 2);
    let (st, hist) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/delivery-orders/history?phone={SARA}&org_id={}&device_token={}",
            s.org,
            device_token(SARA)
        )),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hist.as_array().unwrap().len(), 4);
}

// ── D: erase, leave ─────────────────────────────────────────────────────────

/// After an erase, NO text column of any org-scoped table holds the person's
/// name or phone — including what came in under a customer merged into them.
#[sqlx::test]
async fn erase_leaves_no_personal_data_anywhere(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let tok = admin_token(s.admin, s.org);
    const NAME: &str = "Zephyrine Quillfeather";
    const DUP_NAME: &str = "Zeph Duplicatus";
    const DUP: &str = "01155566677";

    // The person: a loyalty member with orders, a booking, a table bill.
    sqlx::query("INSERT INTO loyalty_settings (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, require_otp, stamp_per_line_item) VALUES ($1, NULL, true, 'points', 1000, 100, false, false)")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let member = seed_loyalty_member(&pool, s.org, SARA, NAME, "tok-erase-1").await;
    let (st, d) = place(
        &app,
        &outside_order(&s, NAME, SARA, "12 Quillfeather Lane", 30.001, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(d["customer_id"], json!(member));
    let (st, f) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!(
                "/delivery-orders/{}/finalize",
                d["id"].as_str().unwrap()
            )),
            &teller_token(s.teller, s.org, s.branch),
        )
        .set_json(json!({ "shift_id": s.till, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{f}");
    let at = chrono::Utc::now() + chrono::Duration::days(1);
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &tok).set_json(json!({
            "branch_id": s.branch, "party_size": 2, "starts_at": at, "guest_name": NAME,
            "guest_phone": SARA, "force": true, "table_ids": [], "send_confirmation": false
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // A duplicate under another phone, with its own order — merged in.
    let (st, d2) = place(
        &app,
        &outside_order(&s, DUP_NAME, DUP, "99 Duplicatus Road", 30.003, 31.003),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d2}");
    let dup_id = d2["customer_id"].as_str().unwrap().to_string();
    let (st, m) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/customers/{dup_id}/merge")),
            &tok,
        )
        .set_json(json!({ "into": member })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM delivery_orders WHERE customer_id = '{member}'")
        )
        .await,
        2,
        "the merge re-pointed the duplicate's delivery order"
    );
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM customer_addresses WHERE customer_id = '{member}'")
        )
        .await,
        2
    );
    // An OTP row for their number.
    sqlx::query("INSERT INTO delivery_otp (phone, code, expires_at) VALUES ($1, '1234', now() + interval '5 minutes')")
        .bind(SARA_KEY)
        .execute(&pool)
        .await
        .unwrap();
    // A till that was offline lands one more row under the MERGED id afterwards.
    sqlx::query("UPDATE delivery_orders SET customer_id = $1 WHERE customer_name = $2")
        .bind(u(&dup_id))
        .bind(DUP_NAME)
        .execute(&pool)
        .await
        .unwrap();

    let money_before = i64_of(
        &pool,
        "SELECT COALESCE(sum(total_amount),0)::bigint FROM orders",
    )
    .await;
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/customers/{member}/erase")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Every text-ish column of every table that carries an org_id, plus the
    // tables keyed by phone or hanging off an order.
    let cols: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.table_name::text, c.column_name::text
           FROM information_schema.columns c
           JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name
          WHERE c.table_schema = 'public' AND t.table_type = 'BASE TABLE'
            AND c.data_type IN ('text', 'character varying', 'jsonb', 'json')
            AND (c.table_name IN ('orders', 'order_items', 'delivery_otp', 'loyalty_transactions', 'loyalty_pass_devices')
                 OR EXISTS (SELECT 1 FROM information_schema.columns o
                             WHERE o.table_schema = 'public' AND o.table_name = c.table_name
                               AND o.column_name = 'org_id'))",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        cols.len() > 100,
        "the sweep must be reading the schema ({} columns)",
        cols.len()
    );
    let needles = [
        NAME,
        "Quillfeather",
        DUP_NAME,
        "Duplicatus",
        SARA_KEY,
        "1000000000",
        "201155566677",
        "1155566677",
    ];
    let mut hits = Vec::new();
    for (table, column) in &cols {
        for n in needles {
            let found: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM \"{table}\" WHERE \"{column}\"::text ILIKE '%' || $1 || '%'"
            ))
            .bind(n)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("{table}.{column}: {e}"));
            if found > 0 {
                hits.push(format!("{table}.{column} still holds {n:?} ({found} rows)"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "erase left personal data behind:\n{}",
        hits.join("\n")
    );

    // Money and the ledger stay.
    assert_eq!(
        i64_of(
            &pool,
            "SELECT COALESCE(sum(total_amount),0)::bigint FROM orders"
        )
        .await,
        money_before
    );
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM delivery_orders").await,
        2
    );
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM bookings").await, 1);
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM loyalty_customers WHERE deleted_at IS NULL"
        )
        .await,
        0
    );
    // The number is free again.
    assert!(customer_of_phone(&pool, s.org, SARA_KEY).await.is_none());
}

/// `DELETE /loyalty/members/{id}` ends the card and keeps the person.
#[sqlx::test]
async fn leaving_the_programme_keeps_the_customer(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let tok = admin_token(s.admin, s.org);
    let member = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-leave-1").await;
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara", SARA, "12 Tahrir St", 30.001, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    sqlx::query("INSERT INTO loyalty_pass_cache (customer_id, org_id, kind, bytes, fingerprint) VALUES ($1,$2,'apple','x','f')")
        .bind(member)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();

    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::delete().uri(&format!("/loyalty/members/{member}")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (name, phone, erased): (String, Option<String>, bool) = sqlx::query_as(
        "SELECT name, phone_key, erased_at IS NOT NULL FROM customers WHERE id = $1",
    )
    .bind(member)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (name.as_str(), phone.as_deref(), erased),
        ("Sara", Some(SARA_KEY), false),
        "the customer is untouched"
    );
    assert_eq!(i64_of(&pool, &format!("SELECT count(*) FROM delivery_orders WHERE customer_id = '{member}' AND customer_name = 'Sara'")).await, 1);
    assert_eq!(i64_of(&pool, &format!("SELECT count(*) FROM customer_addresses WHERE customer_id = '{member}' AND erased_at IS NULL")).await, 1);
    let (deleted, voided, token): (bool, bool, String) = sqlx::query_as(
        "SELECT deleted_at IS NOT NULL, pass_voided_at IS NOT NULL, member_token FROM loyalty_customers WHERE id = $1",
    )
    .bind(member)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(deleted && voided);
    assert_ne!(token, "tok-leave-1", "the old barcode resolves to nobody");
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM loyalty_pass_cache").await,
        0,
        "the stored pass is gone"
    );
    let (st, _) = send(
        &app,
        test::TestRequest::get().uri("/public/loyalty/card/tok-leave-1"),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, c) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/customers/{member}")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(c["customer"]["is_member"], json!(false));
    // Twice is not a failure.
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::delete().uri(&format!("/loyalty/members/{member}")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

// ── E: the voided Apple pass ────────────────────────────────────────────────

/// A retired card keeps authenticating on the Apple web service and is listed
/// as changed, so the device comes back for it; what it is served is marked
/// `voided`. (The signed bytes need Apple credentials, so the archive itself is
/// asserted through `mark_voided`, and the route up to the signer.)
#[sqlx::test]
async fn a_retired_card_is_served_voided_not_404(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let member = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-void-1").await;
    sqlx::query("UPDATE loyalty_customers SET apple_auth_token = 'apple-secret', apple_serial = $1::text WHERE id = $1")
        .bind(member)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO loyalty_pass_devices (device_library_id, customer_id, org_id, push_token) VALUES ('dev-1', $1, $2, 'push')")
        .bind(member)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::delete().uri(&format!("/loyalty/members/{member}")),
            &admin_token(s.admin, s.org),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // The device registration and the auth token outlive the membership …
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM loyalty_pass_devices").await,
        1
    );
    // … the serial is reported as changed …
    let (st, serials) = send(
        &app,
        test::TestRequest::get().uri("/wallet/v1/devices/dev-1/registrations/pass.test"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{serials}");
    assert_eq!(serials["serialNumbers"], json!([member.to_string()]));
    // … and the pass endpoint authenticates the retired card instead of 404ing.
    // A wrong token is still refused.
    let pass = |token: &str| {
        test::TestRequest::get()
            .uri(&format!("/wallet/v1/passes/pass.test/{member}"))
            .insert_header(("Authorization", format!("ApplePass {token}")))
    };
    let (st, _) = send(&app, pass("nope")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, body) = send(&app, pass("apple-secret")).await;
    // Without signing credentials in the test environment the builder answers
    // 503 "not configured" — which is PAST authentication and lookup. With
    // them it is a 200 pkpass. Never the old 404/401.
    assert!(
        st == StatusCode::OK || st == StatusCode::SERVICE_UNAVAILABLE,
        "a retired card must reach the pass builder, got {st}: {body}"
    );

    // What that builder marks.
    let mut pass_json = json!({ "serialNumber": member.to_string() });
    madar_rust::loyalty::wallet::apple::mark_voided(&mut pass_json, chrono::Utc::now());
    assert_eq!(pass_json["voided"], json!(true));
    assert!(pass_json["expirationDate"].is_string());

    // An ERASED member whose card no phone holds is unreachable at once.
    let erased = seed_loyalty_member(&pool, s.org, "01234567890", "Gone", "tok-void-2").await;
    sqlx::query("UPDATE loyalty_customers SET apple_auth_token = 'other-secret' WHERE id = $1")
        .bind(erased)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/customers/{erased}/erase")),
            &admin_token(s.admin, s.org),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = send(
        &app,
        test::TestRequest::get()
            .uri(&format!("/wallet/v1/passes/pass.test/{erased}"))
            .insert_header(("Authorization", "ApplePass other-secret")),
    )
    .await;
    assert!(
        st == StatusCode::NOT_FOUND || st == StatusCode::UNAUTHORIZED,
        "{st}"
    );
}

/// An erased member's card that IS in a wallet is voided before it is
/// forgotten: the device can collect a voided copy (built from the scrubbed row),
/// and then — or after the grace — the token and registrations are purged.
#[sqlx::test]
async fn an_erased_card_in_a_wallet_is_voided_then_purged(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let member = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-era-1").await;
    sqlx::query("UPDATE loyalty_customers SET apple_auth_token = 'apple-secret', apple_serial = $1::text WHERE id = $1")
        .bind(member)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO loyalty_pass_devices (device_library_id, customer_id, org_id, push_token) VALUES ('dev-e', $1, $2, 'push')")
        .bind(member)
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/customers/{member}/erase")),
            &admin_token(s.admin, s.org),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // The device is told the serial changed, and reaches the pass builder.
    let (st, serials) = send(
        &app,
        test::TestRequest::get().uri("/wallet/v1/devices/dev-e/registrations/pass.test"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{serials}");
    assert_eq!(serials["serialNumbers"], json!([member.to_string()]));
    let pass = || {
        test::TestRequest::get()
            .uri(&format!("/wallet/v1/passes/pass.test/{member}"))
            .insert_header(("Authorization", "ApplePass apple-secret"))
    };
    let (st, body) = send(&app, pass()).await;
    assert!(
        st == StatusCode::OK || st == StatusCode::SERVICE_UNAVAILABLE,
        "an erased card in a wallet reaches the (voided) builder, got {st}: {body}"
    );
    // What it would be built from names nobody.
    let row = madar_rust::loyalty::model::find_voided(&pool, member)
        .await
        .unwrap()
        .expect("still served voided");
    assert_eq!(row.name, "");
    assert!(
        !format!("{row:?}").contains("1000000000"),
        "no phone: {row:?}"
    );

    if st == StatusCode::SERVICE_UNAVAILABLE {
        // No signing credentials here, so nothing was served and nothing was
        // purged: young, the sweep leaves it; past the grace, it goes.
        assert_eq!(
            madar_rust::loyalty::model::purge_stale_erased_passes(&pool)
                .await
                .unwrap(),
            0
        );
        sqlx::query("UPDATE customers SET erased_at = now() - interval '49 hours' WHERE id = $1")
            .bind(member)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            madar_rust::loyalty::model::purge_stale_erased_passes(&pool)
                .await
                .unwrap(),
            1
        );
    }
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM loyalty_pass_devices").await,
        0
    );
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM loyalty_customers WHERE apple_auth_token IS NOT NULL OR pass_voided_at IS NOT NULL").await,
        0
    );
    let (st, _) = send(&app, pass()).await;
    assert!(
        st == StatusCode::NOT_FOUND || st == StatusCode::UNAUTHORIZED,
        "{st}"
    );
    // A leaver is never purged by this: only the erased.
    assert!(
        !madar_rust::loyalty::model::purge_erased_pass(&pool, Uuid::new_v4())
            .await
            .unwrap()
    );
}

// ── F: order now ────────────────────────────────────────────────────────────

/// Token only → masked. The masked JSON contains no address, no full phone, no
/// customer id, no history. Token + the right device → everything.
#[sqlx::test]
async fn order_now_is_masked_without_the_device_and_full_with_it(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let member = seed_loyalty_member(&pool, s.org, SARA, "Sara Mostafa", "tok-now-1").await;
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara Mostafa", SARA, "12 Tahrir St", 30.001, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");

    let (st, masked) = send(
        &app,
        test::TestRequest::get().uri("/public/order-now/tok-now-1"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{masked}");
    assert_eq!(masked["verify_required"], json!(true));
    assert_eq!(masked["first_name"], "Sara");
    assert_eq!(masked["phone_hint"], "•••• 0000");
    assert_eq!(masked["last_branch_name"], "Maadi");
    let text = masked.to_string();
    for secret_bit in [
        SARA_KEY,
        "1000000000",
        "Mostafa",
        "Tahrir",
        "addresses",
        "customer_id",
        &member.to_string(),
        &s.branch.to_string(),
        "30.001",
    ] {
        assert!(
            !text.contains(secret_bit),
            "the masked context leaks {secret_bit:?}: {text}"
        );
    }
    assert!(masked.get("full").is_none());

    // A device token for SOMEONE ELSE's phone is no better than none.
    let (st, other) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/order-now/tok-now-1?device_token={}",
            device_token("01155566677")
        )),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(other["verify_required"], json!(true));
    assert!(other.get("full").is_none());

    let (st, full) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/order-now/tok-now-1?device_token={}",
            device_token(SARA)
        )),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{full}");
    assert_eq!(full["verify_required"], json!(false));
    let f = &full["full"];
    assert_eq!(f["customer_id"], json!(member));
    assert_eq!(f["name"], "Sara Mostafa");
    assert_eq!(f["phone"], SARA_KEY);
    assert_eq!(f["last_payment_hint"], "card");
    assert_eq!(f["last_branch"]["id"], json!(s.branch));
    assert_eq!(f["last_branch"]["channel"], "outside");
    assert_eq!(f["last_branch"]["stale"], json!(false), "{f}");
    assert_eq!(f["addresses"][0]["address_line"], "12 Tahrir St");
    assert_eq!(f["addresses"][0]["stale"], json!(false));

    // Unknown token, and another org's token on this org's order: 404.
    let (st, _) = send(&app, test::TestRequest::get().uri("/public/order-now/nope")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

/// Stale is FLAGGED, not dropped: a closed channel and a shrunken zone.
#[sqlx::test]
async fn order_now_flags_a_stale_branch_and_a_stale_address(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-now-2").await;
    // ~2.2 km from the branch.
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara", SARA, "Far Street", 30.02, 31.0),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    let uri = format!(
        "/public/order-now/tok-now-2?device_token={}",
        device_token(SARA)
    );

    // The shop shrinks its delivery ring to 1 km and closes the channel.
    sqlx::query("UPDATE delivery_zones SET max_road_distance_meters = 1000 WHERE branch_id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let (_, full) = send(&app, test::TestRequest::get().uri(&uri)).await;
    assert_eq!(full["full"]["addresses"][0]["stale"], json!(true), "{full}");
    assert_eq!(full["full"]["addresses"][0]["stale_reason"], "out_of_zone");
    assert_eq!(
        full["full"]["addresses"][0]["address_line"], "Far Street",
        "kept, not dropped"
    );
    assert_eq!(full["full"]["last_branch"]["stale"], json!(false));

    sqlx::query(
        "UPDATE branch_delivery_settings SET outside_override = 'closed' WHERE branch_id = $1",
    )
    .bind(s.branch)
    .execute(&pool)
    .await
    .unwrap();
    let (_, full) = send(&app, test::TestRequest::get().uri(&uri)).await;
    assert_eq!(full["full"]["last_branch"]["stale"], json!(true), "{full}");
    assert_eq!(
        full["full"]["last_branch"]["stale_reason"],
        "channel_closed"
    );

    sqlx::query("UPDATE branches SET is_active = false WHERE id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let (_, full) = send(&app, test::TestRequest::get().uri(&uri)).await;
    assert_eq!(
        full["full"]["last_branch"]["stale_reason"],
        "branch_unavailable"
    );
    assert_eq!(
        full["full"]["addresses"][0]["stale_reason"],
        "branch_unavailable"
    );
}

/// §4.4 at order create: the server classifies the typed contact.
#[sqlx::test]
async fn ordering_from_a_card_classifies_the_typed_identity(pool: PgPool) {
    let s = shop(&pool, true).await;
    let app = app!(pool);
    let member = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-id-1").await;
    const FRIEND: &str = "01155566677";
    let card_order = |name: &str, phone: &str, address: &str| {
        let mut b = outside_order(&s, name, phone, address, 30.001, 31.001);
        b["member_token"] = json!("tok-id-1");
        b["device_token"] = json!(device_token(SARA));
        b
    };

    // The token alone orders nothing: the device must prove the CUSTOMER's phone.
    let mut stolen = card_order("Sara", SARA, "1 A St");
    stolen["device_token"] = json!(device_token(FRIEND));
    let (st, e) = place(&app, &stolen).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(
        e["code"].is_null(),
        "an unproven CARD phone stays a bare 401: {e}"
    );

    // Nothing differs: an ordinary order, address saved.
    let (st, d) = place(&app, &card_order(" sara ", "+201000000000", "1 A St")).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(d["customer_id"], json!(member));
    assert!(d["address_id"].is_string());

    // A different phone, no choice → 409 with the kind.
    let (st, e) = place(&app, &card_order("Omar", FRIEND, "2 B St")).await;
    assert_eq!(st, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["code"], "IDENTITY_CHOICE_REQUIRED");
    assert_eq!(e["kind"], "phone");
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM delivery_orders").await,
        1,
        "nothing was placed"
    );

    // One-time, at a branch that requires OTP: the snapshot phone needs proof.
    let mut one = card_order("Omar", FRIEND, "2 B St");
    one["identity_change"] = json!("one_time");
    let (st, e) = place(&app, &one).await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "the branch's OTP rule applies to the number the driver calls"
    );
    assert_eq!(
        e["code"], "CONTACT_VERIFICATION_REQUIRED",
        "named, so the client asks for an OTP on the OTHER number: {e}"
    );
    // A token for the wrong number is the same ask, not a sign-out.
    one["contact_device_token"] = json!(device_token(SARA));
    let (st, e) = place(&app, &one).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(e["code"], "CONTACT_VERIFICATION_REQUIRED", "{e}");
    one["contact_device_token"] = json!(device_token(FRIEND));
    let (st, d) = place(&app, &one).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(
        d["customer_id"],
        json!(member),
        "the order stays the card owner's"
    );
    assert_eq!(d["customer_name"], "Omar");
    assert_eq!(d["customer_phone"], "201155566677");
    assert_eq!(d["contact_override"], json!(true));
    assert!(
        d["address_id"].is_null(),
        "someone else's address is not saved by default"
    );
    assert!(
        customer_of_phone(&pool, s.org, "201155566677")
            .await
            .is_none(),
        "and no customer is made of the friend"
    );

    // … unless they tick "save this address".
    let mut keep = card_order("Omar", FRIEND, "3 C St");
    keep["identity_change"] = json!("one_time");
    keep["contact_device_token"] = json!(device_token(FRIEND));
    keep["save_address"] = json!(true);
    let (st, d) = place(&app, &keep).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert!(d["address_id"].is_string());

    // A name alone, unsaid: one-time — snapshot only, the profile keeps its name.
    let (st, d) = place(&app, &card_order("Sara Mostafa", SARA, "1 A St")).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(d["customer_name"], "Sara Mostafa");
    assert_eq!(d["contact_override"], json!(false));
    assert_eq!(
        customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap().1,
        "Sara"
    );

    // update_name: the stored name changes, audited as the customer's own act.
    let mut rename = card_order("Sara Mostafa", SARA, "1 A St");
    rename["identity_change"] = json!("update_name");
    let (st, d) = place(&app, &rename).await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(
        customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap().1,
        "Sara Mostafa"
    );
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM customer_identity_audit WHERE kind = 'name' AND actor_kind = 'customer' AND old_value = 'Sara' AND new_value = 'Sara Mostafa'").await,
        1
    );

    // Garbage in `identity_change` is a 400, not a guess.
    let mut junk = card_order("Sara Mostafa", SARA, "1 A St");
    junk["identity_change"] = json!("replace");
    let (st, _) = place(&app, &junk).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// Replace identity: both proofs or nothing; history, audit, limits; and the
/// old phone's device stops authorising.
#[sqlx::test]
async fn replacing_identity_needs_both_phones_and_is_rate_limited(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let member = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-rep-1").await;
    const NEW1: &str = "01155566677";
    const NEW2: &str = "01222333444";
    const NEW3: &str = "01099988877";
    let replace = |cur: &str, new: &str, new_proof: &str| {
        test::TestRequest::post()
            .uri("/public/order-now/tok-rep-1/replace-identity")
            .set_json(json!({ "device_token": device_token(cur), "new_phone": new, "new_phone_device_token": new_proof }))
    };

    // Only the current phone proven / only the new one proven → nothing changes.
    let (st, _) = send(&app, replace(SARA, NEW1, "garbage")).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = send(&app, replace(NEW1, NEW1, &device_token(NEW1))).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(
        customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap().0,
        member
    );

    let (st, r) = send(&app, replace(SARA, NEW1, &device_token(NEW1))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["customer_id"], json!(member), "same id");
    assert_eq!(r["phone_hint"], "•••• 6677");
    let text = r.to_string();
    assert!(
        !text.contains("201155566677") && !text.contains("device_token"),
        "nothing secret comes back: {text}"
    );
    assert_eq!(
        customer_of_phone(&pool, s.org, "201155566677")
            .await
            .unwrap()
            .0,
        member
    );
    assert!(customer_of_phone(&pool, s.org, SARA_KEY).await.is_none());
    assert_eq!(
        i64_of(&pool, &format!("SELECT count(*) FROM customer_phone_history WHERE customer_id = '{member}' AND phone_key = '{SARA_KEY}' AND reason = 'self' AND replaced_by IS NULL")).await,
        1
    );
    assert_eq!(i64_of(&pool, "SELECT count(*) FROM customer_identity_audit WHERE kind = 'phone' AND actor_kind = 'customer' AND actor_user IS NULL").await, 1);

    // The OLD phone's device no longer unlocks this customer.
    let (_, ctx) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/order-now/tok-rep-1?device_token={}",
            device_token(SARA)
        )),
    )
    .await;
    assert_eq!(ctx["verify_required"], json!(true));
    let (_, ctx) = send(
        &app,
        test::TestRequest::get().uri(&format!(
            "/public/order-now/tok-rep-1?device_token={}",
            device_token(NEW1)
        )),
    )
    .await;
    assert_eq!(ctx["verify_required"], json!(false));

    // A second is allowed; a third within 30 days is not.
    let (st, r) = send(&app, replace(NEW1, NEW2, &device_token(NEW2))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (st, e) = send(&app, replace(NEW2, NEW3, &device_token(NEW3))).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{e}");
    assert_eq!(e["code"], "IDENTITY_REPLACE_LIMIT");
    // … and the window ROLLS: a month later it is allowed again.
    sqlx::query("UPDATE customer_phone_history SET replaced_at = now() - interval '31 days'")
        .execute(&pool)
        .await
        .unwrap();
    let (st, r) = send(&app, replace(NEW2, NEW3, &device_token(NEW3))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
}

/// The new phone is another customer's: 409 + can_combine; combine merges with
/// the PASS HOLDER surviving — both-members rule included — and then locks
/// identity changes for 24 h.
#[sqlx::test]
async fn combining_two_profiles_keeps_the_pass_holder(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    const OTHER: &str = "01155566677";
    const OTHER_KEY: &str = "201155566677";
    sqlx::query("INSERT INTO loyalty_settings (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, require_otp, stamp_per_line_item) VALUES ($1, NULL, true, 'points', 1000, 100, false, false)")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let me = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-comb-me").await;
    let other = seed_loyalty_member(&pool, s.org, OTHER, "Sara M", "tok-comb-other").await;
    for (member, points) in [(me, 30), (other, 50)] {
        sqlx::query(
            "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, source) \
             VALUES ($1, $2, $3, 'adjust', 'points', $4, 'manual')",
        )
        .bind(s.org)
        .bind(member)
        .bind(s.branch)
        .bind(points)
        .execute(&pool)
        .await
        .unwrap();
    }
    // The other profile has an order and an address of its own.
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara M", OTHER, "5 Other St", 30.001, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    assert_eq!(d["customer_id"], json!(other));

    let body = json!({ "device_token": device_token(SARA), "new_phone": OTHER, "new_phone_device_token": device_token(OTHER) });
    let (st, e) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/order-now/tok-comb-me/replace-identity")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["code"], "PHONE_BELONGS_TO_ANOTHER");
    assert_eq!(e["can_combine"], json!(true));
    assert_eq!(
        customer_of_phone(&pool, s.org, SARA_KEY).await.unwrap().0,
        me,
        "declining leaves both untouched"
    );
    assert_eq!(
        customer_of_phone(&pool, s.org, OTHER_KEY).await.unwrap().0,
        other
    );

    // Combining needs the same two proofs.
    let mut weak = body.clone();
    weak["new_phone_device_token"] = json!("garbage");
    let (st, _) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/order-now/tok-comb-me/combine")
            .set_json(&weak),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let (st, r) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/order-now/tok-comb-me/combine")
            .set_json(&body),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["customer_id"], json!(me), "the pass holder survives");
    assert_eq!(r["combined"], json!(true));
    let merged_into: Option<Uuid> =
        sqlx::query_scalar("SELECT merged_into FROM customers WHERE id = $1")
            .bind(other)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(merged_into, Some(me));
    // Both were members: the balance crossed, the other card is retired + aliased.
    let balance: i32 =
        sqlx::query_scalar("SELECT points_balance FROM loyalty_customers WHERE id = $1")
            .bind(me)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(balance, 80);
    assert_eq!(i64_of(&pool, &format!("SELECT count(*) FROM loyalty_customers WHERE id = '{other}' AND deleted_at IS NOT NULL AND pass_voided_at IS NOT NULL")).await, 1);
    let (st, card) = send(
        &app,
        test::TestRequest::get().uri("/public/order-now/tok-comb-other"),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the retired card's token resolves to the survivor: {card}"
    );
    // "This is my new number": it is THE number now; the old one is history.
    assert_eq!(
        customer_of_phone(&pool, s.org, OTHER_KEY).await.unwrap().0,
        me
    );
    assert_eq!(i64_of(&pool, &format!("SELECT count(*) FROM customer_phone_history WHERE customer_id = '{me}' AND phone_key = '{SARA_KEY}'")).await, 1);
    assert_eq!(i64_of(&pool, &format!("SELECT count(*) FROM customer_phone_history WHERE customer_id = '{me}' AND phone_key = '{OTHER_KEY}'")).await, 0);
    // Orders and addresses followed.
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM delivery_orders WHERE customer_id = '{me}'")
        )
        .await,
        1
    );
    assert_eq!(
        i64_of(
            &pool,
            &format!("SELECT count(*) FROM customer_addresses WHERE customer_id = '{me}'")
        )
        .await,
        1
    );

    // Locked for 24 h after a merge.
    let again = json!({ "device_token": device_token(OTHER), "new_phone": "01222333444", "new_phone_device_token": device_token("01222333444") });
    let (st, e) = send(
        &app,
        test::TestRequest::post()
            .uri("/public/order-now/tok-comb-me/replace-identity")
            .set_json(&again),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{e}");
    assert_eq!(e["code"], "IDENTITY_LOCKED_AFTER_MERGE");
}

/// A double tap cannot make two orders.
#[sqlx::test]
async fn a_double_tap_places_one_order(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let key = Uuid::new_v4().to_string();
    let body = outside_order(&s, "Sara", SARA, "12 Tahrir St", 30.001, 31.001);
    let req = || {
        test::TestRequest::post()
            .uri("/public/delivery-orders")
            .insert_header(("Idempotency-Key", key.clone()))
            .set_json(&body)
    };
    let (a, b) = futures::join!(send(&app, req()), send(&app, req()));
    assert!(a.0.is_success() && b.0.is_success(), "{} / {}", a.1, b.1);
    assert_eq!(a.1["id"], b.1["id"]);
    let (st, c) = send(&app, req()).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(c["id"], a.1["id"]);
    assert_eq!(
        i64_of(&pool, "SELECT count(*) FROM delivery_orders").await,
        1
    );
}

/// The link on the card: only with a public ordering base AND a shop that takes
/// online orders; it is part of what makes a stored pass stale.
#[sqlx::test]
async fn the_order_now_link_follows_the_env_and_the_shop(pool: PgPool) {
    let s = shop(&pool, false).await;
    let quiet = seed_org(&pool).await; // no ordering channel anywhere
    let me = seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-link-1").await;
    let them = seed_loyalty_member(&pool, quiet, SARA, "Sara", "tok-link-2").await;
    let me = madar_rust::loyalty::model::find_by_id(&pool, me)
        .await
        .unwrap()
        .unwrap();
    let them = madar_rust::loyalty::model::find_by_id(&pool, them)
        .await
        .unwrap()
        .unwrap();

    // SAFETY: this suite's only test that touches the variable.
    unsafe { std::env::remove_var("PUBLIC_ORDER_BASE_URL") };
    assert_eq!(
        madar_rust::loyalty::wallet::order_now_for(&pool, &me).await,
        None
    );
    unsafe { std::env::set_var("PUBLIC_ORDER_BASE_URL", "https://order.example/") };
    assert_eq!(
        madar_rust::loyalty::wallet::order_now_for(&pool, &me)
            .await
            .as_deref(),
        Some("https://order.example/now/tok-link-1")
    );
    assert_eq!(
        madar_rust::loyalty::wallet::order_now_for(&pool, &them).await,
        None,
        "a shop with no online ordering gets no link"
    );

    // First on the back of the card; Google gets it as a button instead.
    let settings = madar_rust::loyalty::settings::LoyaltySettings::defaults(s.org, None);
    let mut copy = madar_rust::loyalty::wallet::CardCopy::default();
    copy.order_now_url = madar_rust::loyalty::wallet::order_now_for(&pool, &me).await;
    let back = madar_rust::loyalty::wallet::back_of_card(&me, &settings, &copy);
    assert_eq!(back[0].key, "ordernow");
    assert_eq!(back[0].value, "https://order.example/now/tok-link-1");
    copy.order_now_url = None;
    assert_eq!(
        madar_rust::loyalty::wallet::back_of_card(&me, &settings, &copy)[0].key,
        "howitworks"
    );

    // The public card carries it.
    let app = app!(pool);
    let (st, card) = send(
        &app,
        test::TestRequest::get().uri("/public/loyalty/card/tok-link-1"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{card}");
    assert_eq!(
        card["order_now_url"],
        "https://order.example/now/tok-link-1"
    );
    // Switching ordering off removes it.
    sqlx::query("UPDATE branch_delivery_settings SET in_mall_enabled = false, outside_enabled = false WHERE branch_id = $1")
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let (_, card) = send(
        &app,
        test::TestRequest::get().uri("/public/loyalty/card/tok-link-1"),
    )
    .await;
    assert!(card.get("order_now_url").is_none(), "{card}");
    unsafe { std::env::remove_var("PUBLIC_ORDER_BASE_URL") };
}

// ── wave 3: the customer on an open bill, bookings, limits ──────────────────

async fn seed_table(pool: &PgPool, s: &Shop, label: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO branch_tables (id, org_id, branch_id, label) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(s.org)
        .bind(s.branch)
        .bind(label)
        .execute(pool)
        .await
        .unwrap();
    id
}
/// An anonymous bill on `table` (a table-QR order with no phone).
async fn open_bill<S>(app: &S, s: &Shop, table: Uuid) -> Uuid
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let (st, v) = send(
        app,
        test::TestRequest::post()
            .uri("/public/table-orders")
            .set_json(json!({
                "table_id": table, "customer_name": "Walk-in",
                "idempotency_key": Uuid::new_v4(),
                "items": [{ "menu_item_id": s.item, "quantity": 1 }]
            })),
    )
    .await;
    assert!(st.is_success(), "{v}");
    u(v["id"].as_str().unwrap())
}
async fn new_customer<S>(app: &S, tok: &str, name: &str, phone: Option<&str>) -> Uuid
where
    S: Service<Request, Response = ServiceResponse, Error = actix_web::Error>,
{
    let (st, c) = send(
        app,
        auth(test::TestRequest::post().uri("/customers"), tok)
            .set_json(json!({ "name": name, "phone": phone })),
    )
    .await;
    assert!(st.is_success(), "{c}");
    u(c["customer"]["id"].as_str().unwrap())
}
async fn ticket_customer(pool: &PgPool, ticket: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT customer_id FROM open_tickets WHERE id = $1")
        .bind(ticket)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn ticket_seq(pool: &PgPool, ticket: Uuid) -> i64 {
    sqlx::query_scalar("SELECT seq FROM sync_changes WHERE type = 'open_ticket' AND entity_id = $1")
        .bind(ticket)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A customer picked on a bill that is ALREADY open reaches the server — and
/// the other tills — at once, through replay and through REST alike; and no
/// shape of it is ever an error a till's outbox could wedge on.
#[sqlx::test]
async fn the_customer_on_an_open_bill_is_set_changed_and_cleared(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let admin = admin_token(s.admin, s.org);
    let tok = teller_token(s.teller, s.org, s.branch);
    let table = seed_table(&pool, &s, "T1").await;
    let ticket = open_bill(&app, &s, table).await;
    assert_eq!(ticket_customer(&pool, ticket).await, None);

    let sara = new_customer(&app, &admin, "Sara", Some(SARA)).await;
    let omar = new_customer(&app, &admin, "Omar", Some("01155566677")).await;
    let dupe = new_customer(&app, &admin, "Omar K", None).await;
    let (st, m) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/customers/{dupe}/merge")),
            &admin,
        )
        .set_json(json!({ "into": omar })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");

    let replay = |customer: Value| {
        auth(test::TestRequest::post().uri("/sync/replay"), &tok).set_json(json!({
            "op": "set_ticket_customer", "teller_id": s.teller,
            "ticket_id": ticket, "customer_id": customer
        }))
    };

    // Set: the row, the response, and the changefeed all move.
    let before = ticket_seq(&pool, ticket).await;
    let (st, r) = send(&app, replay(json!(sara))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["ticket_id"], json!(ticket));
    assert_eq!(r["customer_id"], json!(sara));
    assert_eq!(r["applied"], json!(true));
    assert!(r["order_id"].is_null());
    assert_eq!(ticket_customer(&pool, ticket).await, Some(sara));
    let after = ticket_seq(&pool, ticket).await;
    assert!(
        after > before,
        "the other tills are told: {before} -> {after}"
    );

    // A re-flushed queue is the same row and no second feed entry.
    let (st, r) = send(&app, replay(json!(sara))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["applied"], json!(true));
    assert_eq!(ticket_seq(&pool, ticket).await, after, "idempotent");

    // Last write wins; a MERGED id lands on its survivor.
    let (st, r) = send(&app, replay(json!(dupe))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["customer_id"], json!(omar));
    assert_eq!(ticket_customer(&pool, ticket).await, Some(omar));

    // An unknown id is dropped without touching the bill — and without an error.
    let (st, r) = send(&app, replay(json!(Uuid::new_v4()))).await;
    assert_eq!(st, StatusCode::OK, "a bill is never refused: {r}");
    assert_eq!(r["applied"], json!(false));
    assert_eq!(r["customer_id"], json!(omar), "what the bill still says");
    assert_eq!(ticket_customer(&pool, ticket).await, Some(omar));

    // The pull projection (the ticket view) names the customer.
    let (st, view) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/open-tickets/{ticket}")),
            &tok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(view["customer_id"], json!(omar));

    // null — and an absent field — take the customer off.
    let (st, r) = send(&app, replay(Value::Null)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert!(r["customer_id"].is_null());
    assert_eq!(ticket_customer(&pool, ticket).await, None);
    send(&app, replay(json!(sara))).await;
    let (st, _) = send(
        &app,
        auth(test::TestRequest::post().uri("/sync/replay"), &tok).set_json(
            json!({ "op": "set_ticket_customer", "teller_id": s.teller, "ticket_id": ticket }),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ticket_customer(&pool, ticket).await, None);

    // REST, the dashboard's path: same core, same answers.
    let put = |t: &str, body: Value| {
        auth(
            test::TestRequest::put().uri(&format!("/open-tickets/{ticket}/customer")),
            t,
        )
        .set_json(body)
    };
    let (st, r) = send(&app, put(&admin, json!({ "customer_id": sara }))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["customer_id"], json!(sara));
    assert_eq!(ticket_customer(&pool, ticket).await, Some(sara));
    let (st, r) = send(&app, put(&admin, json!({ "customer_id": null }))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(ticket_customer(&pool, ticket).await, None);

    // Gated by customers.attach — on REST and on replay.
    let kitchen = seed_user(&pool, s.org, "kitchen").await;
    sqlx::query("INSERT INTO user_branch_assignments (user_id, branch_id) VALUES ($1,$2)")
        .bind(kitchen)
        .bind(s.branch)
        .execute(&pool)
        .await
        .unwrap();
    let ktok = create_token(
        &secret(),
        kitchen,
        Some(s.org),
        UserRole::Kitchen,
        Some(s.branch),
        24,
    )
    .unwrap();
    let (st, _) = send(&app, put(&ktok, json!({ "customer_id": sara }))).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = send(
        &app,
        auth(test::TestRequest::post().uri("/sync/replay"), &ktok).set_json(json!({
            "op": "set_ticket_customer", "teller_id": kitchen,
            "ticket_id": ticket, "customer_id": sara
        })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(ticket_customer(&pool, ticket).await, None);
    // Another shop's bill is a 404 on REST.
    let other = shop(&pool, false).await;
    let (st, _) = send(
        &app,
        put(
            &admin_token(other.admin, other.org),
            json!({ "customer_id": sara }),
        ),
    )
    .await;
    assert!(
        st == StatusCode::NOT_FOUND || st == StatusCode::FORBIDDEN,
        "{st}"
    );
    assert_eq!(ticket_customer(&pool, ticket).await, None);
}

/// The pick was queued while the bill was open and arrives after it was
/// settled: it lands on the SALE, as `attach_customer` would have. A voided or
/// unknown bill is a clean no-op. The table's history names the customer.
#[sqlx::test]
async fn a_late_pick_lands_on_the_sale_and_a_dead_bill_is_a_no_op(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let admin = admin_token(s.admin, s.org);
    let tok = teller_token(s.teller, s.org, s.branch);
    let table = seed_table(&pool, &s, "T1").await;
    let ticket = open_bill(&app, &s, table).await;
    let sara = new_customer(&app, &admin, "Sara", Some(SARA)).await;

    let (st, o) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/open-tickets/{ticket}/settle")),
            &tok,
        )
        .set_json(json!({ "shift_id": s.till, "payment_method": "cash" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{o}");
    assert!(o["customer_id"].is_null());
    let order = u(o["id"].as_str().unwrap());

    let replay = |ticket: Uuid, customer: Value| {
        auth(test::TestRequest::post().uri("/sync/replay"), &tok).set_json(json!({
            "op": "set_ticket_customer", "teller_id": s.teller,
            "ticket_id": ticket, "customer_id": customer
        }))
    };
    let (st, r) = send(&app, replay(ticket, json!(sara))).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["applied"], json!(true));
    assert_eq!(r["order_id"], json!(order));
    let on_sale: Option<Uuid> = sqlx::query_scalar("SELECT customer_id FROM orders WHERE id = $1")
        .bind(order)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(on_sale, Some(sara), "the sale belongs to her now");

    // The table's history says who sat there.
    let (st, h) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/floor/tables/{table}/history")),
            &admin,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{h}");
    assert_eq!(h["sittings"][0]["customer_id"], json!(sara), "{h}");

    // A voided bill, and one that never existed: 200, nothing written.
    let table2 = seed_table(&pool, &s, "T2").await;
    let dead = open_bill(&app, &s, table2).await;
    sqlx::query("UPDATE open_tickets SET status = 'voided', voided_at = now() WHERE id = $1")
        .bind(dead)
        .execute(&pool)
        .await
        .unwrap();
    for t in [dead, Uuid::new_v4()] {
        let (st, r) = send(&app, replay(t, json!(sara))).await;
        assert_eq!(
            st,
            StatusCode::OK,
            "never an error that wedges an outbox: {r}"
        );
        assert_eq!(r["applied"], json!(false));
    }
    assert_eq!(ticket_customer(&pool, dead).await, None);
}

/// A customer's bookings: newest first, paged, behind `customers.view`, and a
/// merged id answers with everything booked under either id.
#[sqlx::test]
async fn a_customers_bookings_are_listed_through_the_merge_chain(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let admin = admin_token(s.admin, s.org);
    let at = chrono::Utc::now() + chrono::Duration::days(1);
    let book = |phone: &str, hours: i64| {
        auth(test::TestRequest::post().uri("/bookings"), &admin).set_json(json!({
            "branch_id": s.branch, "party_size": 2,
            "starts_at": at + chrono::Duration::hours(hours), "guest_name": "Sara",
            "guest_phone": phone, "force": true, "table_ids": [], "send_confirmation": false
        }))
    };
    let (st, b1) = send(&app, book(SARA, 0)).await;
    assert_eq!(st, StatusCode::CREATED, "{b1}");
    let (st, b2) = send(&app, book(SARA, 4)).await;
    assert_eq!(st, StatusCode::CREATED, "{b2}");
    let (st, b3) = send(&app, book("01155566677", 8)).await;
    assert_eq!(st, StatusCode::CREATED, "{b3}");
    let sara = u(b1["customer_id"].as_str().unwrap());
    let other = u(b3["customer_id"].as_str().unwrap());
    assert_ne!(sara, other);

    let list = |id: Uuid, q: &str| {
        auth(
            test::TestRequest::get().uri(&format!("/customers/{id}/bookings{q}")),
            &admin,
        )
    };
    let (st, rows) = send(&app, list(sara, "")).await;
    assert_eq!(st, StatusCode::OK, "{rows}");
    let ids: Vec<&str> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![b2["id"].as_str().unwrap(), b1["id"].as_str().unwrap()],
        "newest first, and only hers"
    );
    assert_eq!(
        rows[0]["guest_name"], "Sara",
        "the booking view type: {rows}"
    );

    // Merge the other into her: both ids now answer with all three.
    let (st, m) = send(
        &app,
        auth(
            test::TestRequest::post().uri(&format!("/customers/{other}/merge")),
            &admin,
        )
        .set_json(json!({ "into": sara })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    // Even a reference the merge did not move is found through the chain.
    sqlx::query("UPDATE bookings SET customer_id = $1 WHERE id = $2")
        .bind(other)
        .bind(u(b3["id"].as_str().unwrap()))
        .execute(&pool)
        .await
        .unwrap();
    for id in [sara, other] {
        let (st, rows) = send(&app, list(id, "")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(rows.as_array().unwrap().len(), 3, "{rows}");
        assert_eq!(rows[0]["id"], b3["id"]);
    }
    let (_, page) = send(&app, list(sara, "?limit=1&offset=1")).await;
    assert_eq!(page.as_array().unwrap().len(), 1);
    assert_eq!(page[0]["id"], b2["id"]);

    let (st, _) = send(&app, list(Uuid::new_v4(), "")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let kitchen = seed_user(&pool, s.org, "kitchen").await;
    let ktok = create_token(
        &secret(),
        kitchen,
        Some(s.org),
        UserRole::Kitchen,
        Some(s.branch),
        24,
    )
    .unwrap();
    let (st, _) = send(
        &app,
        auth(
            test::TestRequest::get().uri(&format!("/customers/{sara}/bookings")),
            &ktok,
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

/// Design §4.2: a card has its own budget whatever address asks, and the
/// identity endpoints share a much tighter one. (The test client has no peer
/// address, so every request is "127.0.0.1" to the per-IP limiter: the counts
/// below stay under ITS bursts (rate_limit::ROUTE_LIMITS) so that what refuses
/// is provably the per-card bucket.)
#[sqlx::test]
async fn order_now_is_limited_per_card_as_well_as_per_address(pool: PgPool) {
    let s = shop(&pool, false).await;
    seed_loyalty_member(&pool, s.org, SARA, "Sara", "tok-lim-1").await;
    seed_loyalty_member(&pool, s.org, "01155566677", "Omar", "tok-lim-2").await;
    let app = limited_app!(pool);
    let identity = |token: &str, which: &str| {
        test::TestRequest::post()
            .uri(&format!("/public/order-now/{token}/{which}"))
            .set_json(json!({ "device_token": "x", "new_phone": "01222333444",
                              "new_phone_device_token": "x", "other_phone": "01222333444",
                              "other_device_token": "x" }))
    };
    // A card's tries — spread over BOTH identity endpoints — and the next is
    // refused before it reaches the handler.
    let card = madar_rust::rate_limit::limit_of("CUSTOMERS", "IDENTITY_TOKEN").burst;
    assert!(
        card < madar_rust::rate_limit::limit_of("CUSTOMERS", "IDENTITY").burst,
        "the card's bucket is the tighter one"
    );
    for i in 0..card {
        let which = if i % 2 == 0 {
            "replace-identity"
        } else {
            "combine"
        };
        let (st, _) = send(&app, identity("tok-lim-1", which)).await;
        assert_ne!(st, StatusCode::TOO_MANY_REQUESTS, "try {i}");
    }
    let (st, _) = send(&app, identity("tok-lim-1", "combine")).await;
    assert_eq!(
        st,
        StatusCode::TOO_MANY_REQUESTS,
        "the card's bucket is empty"
    );
    // The same address, ANOTHER card: not refused — so it was the card's
    // bucket, not the address's (whose burst is larger).
    let (st, _) = send(&app, identity("tok-lim-2", "replace-identity")).await;
    assert_ne!(st, StatusCode::TOO_MANY_REQUESTS);

    // Browsing: the card's burst, under the address's.
    let card = madar_rust::rate_limit::limit_of("CUSTOMERS", "BROWSE_TOKEN").burst;
    assert!(card < madar_rust::rate_limit::limit_of("CUSTOMERS", "BROWSE").burst);
    for i in 0..card {
        let (st, _) = send(
            &app,
            test::TestRequest::get().uri("/public/order-now/tok-lim-1"),
        )
        .await;
        assert_ne!(st, StatusCode::TOO_MANY_REQUESTS, "view {i}");
    }
    let (st, _) = send(
        &app,
        test::TestRequest::get().uri("/public/order-now/tok-lim-1"),
    )
    .await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS);
    let (st, _) = send(
        &app,
        test::TestRequest::get().uri("/public/order-now/tok-lim-2"),
    )
    .await;
    assert_ne!(
        st,
        StatusCode::TOO_MANY_REQUESTS,
        "another card still opens"
    );
}

/// Design §2.5: what still names a customer merged more than 30 days ago is
/// moved to the survivor — at the END of the chain — and a younger merge is
/// left alone. Twice is the same as once.
#[sqlx::test]
async fn the_nightly_job_repoints_references_to_long_merged_customers(pool: PgPool) {
    let s = shop(&pool, false).await;
    let app = app!(pool);
    let admin = admin_token(s.admin, s.org);
    let keep = new_customer(&app, &admin, "Sara", Some(SARA)).await;
    let mid = new_customer(&app, &admin, "Sara M", None).await;
    let old = new_customer(&app, &admin, "S. Mostafa", None).await;
    let young = new_customer(&app, &admin, "Sarah", None).await;
    for (from, into) in [(old, mid), (mid, keep), (young, keep)] {
        let (st, m) = send(
            &app,
            auth(
                test::TestRequest::post().uri(&format!("/customers/{from}/merge")),
                &admin,
            )
            .set_json(json!({ "into": into })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{m}");
    }
    sqlx::query("UPDATE customers SET merged_at = now() - interval '31 days' WHERE id = ANY($1)")
        .bind(vec![old, mid])
        .execute(&pool)
        .await
        .unwrap();

    // Rows that arrived AFTER the merges, still naming the duplicates: a bill
    // and a sale from a till that was offline, a booking, a saved address.
    let table = seed_table(&pool, &s, "T1").await;
    let bill = open_bill(&app, &s, table).await;
    let late = open_bill(&app, &s, seed_table(&pool, &s, "T2").await).await;
    sqlx::query("UPDATE open_tickets SET customer_id = $1 WHERE id = $2")
        .bind(old)
        .bind(bill)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE open_tickets SET customer_id = $1 WHERE id = $2")
        .bind(young)
        .bind(late)
        .execute(&pool)
        .await
        .unwrap();
    let (st, b) = send(
        &app,
        auth(test::TestRequest::post().uri("/bookings"), &admin).set_json(json!({
            "branch_id": s.branch, "party_size": 2,
            "starts_at": chrono::Utc::now() + chrono::Duration::days(1), "guest_name": "Sara",
            "guest_phone": SARA, "force": true, "table_ids": [], "send_confirmation": false
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    sqlx::query("UPDATE bookings SET customer_id = $1")
        .bind(mid)
        .execute(&pool)
        .await
        .unwrap();
    let (st, d) = place(
        &app,
        &outside_order(&s, "Sara", SARA, "1 A St", 30.001, 31.001),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{d}");
    sqlx::query("UPDATE delivery_orders SET customer_id = $1")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE orders SET customer_id = $1 WHERE customer_id IS NOT NULL")
        .bind(mid)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE customer_addresses SET customer_id = $1")
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();

    let moved = madar_rust::customers::repoint::run_tick(&pool)
        .await
        .unwrap();
    assert_eq!(moved.customers, 2, "{moved:?}");
    for table in [
        "orders",
        "delivery_orders",
        "bookings",
        "customer_addresses",
    ] {
        assert_eq!(
            i64_of(
                &pool,
                &format!(
                    "SELECT count(*) FROM {table} WHERE customer_id IS NOT NULL AND customer_id <> '{keep}'"
                )
            )
            .await,
            0,
            "{table} names only the survivor"
        );
    }
    assert_eq!(ticket_customer(&pool, bill).await, Some(keep));
    assert_eq!(
        ticket_customer(&pool, late).await,
        Some(young),
        "a merge younger than 30 days is left to the chain"
    );
    assert_eq!(
        i64_of(
            &pool,
            "SELECT count(*) FROM customer_addresses WHERE erased_at IS NULL"
        )
        .await,
        1,
        "the address moved, it was not lost"
    );

    let again = madar_rust::customers::repoint::run_tick(&pool)
        .await
        .unwrap();
    assert_eq!(again, Default::default(), "idempotent");
}

// ── A: the seeded migration ─────────────────────────────────────────────────

fn subset(pred: impl Fn(i64) -> bool) -> Migrator {
    let migrations: Vec<_> = MIGRATOR
        .iter()
        .filter(|m| pred(m.version))
        .cloned()
        .collect();
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        locking: true,
        no_tx: false,
    }
}

struct FreshDb {
    name: String,
    base: sqlx::postgres::PgConnectOptions,
}
impl Drop for FreshDb {
    fn drop(&mut self) {
        let name = std::mem::take(&mut self.name);
        let opts = self.base.clone().database("postgres");
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                use sqlx::Connection;
                if let Ok(mut conn) = sqlx::PgConnection::connect_with(&opts).await {
                    let _ =
                        sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                }
            });
        })
        .join();
    }
}
async fn fresh(pool: &PgPool) -> (PgPool, FreshDb) {
    let name = format!("_sqlx_test_t0_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\" TEMPLATE template0"))
        .execute(pool)
        .await
        .expect("create fresh database");
    let base = pool.connect_options().as_ref().clone();
    let fresh = sqlx::pool::PoolOptions::new()
        .max_connections(4)
        .connect_with(base.clone().database(&name))
        .await
        .expect("connect fresh database");
    (fresh, FreshDb { name, base })
}

/// Seed the world as it stood after wave 1, run the wave-2 migrations, and
/// check every link the backfill promises.
#[sqlx::test]
async fn the_references_backfill_links_seeded_rows(pool: PgPool) {
    let (db, _guard) = fresh(&pool).await;
    for role in ["sufrix", "madar_app"] {
        let _ = sqlx::raw_sql(&format!("CREATE ROLE {role} NOLOGIN"))
            .execute(&db)
            .await;
    }
    subset(|v| v < REFERENCES)
        .run(&db)
        .await
        .expect("migrations before wave 2");

    let org = u("00000000-0000-4000-8000-0000000d0001");
    let org2 = u("00000000-0000-4000-8000-0000000d0009");
    let branch = u("00000000-0000-4000-8000-0000000d0002");
    let branch2 = u("00000000-0000-4000-8000-0000000d000a");
    let teller = u("00000000-0000-4000-8000-0000000d0003");
    let known = u("00000000-0000-4000-8000-0000000d0011"); // an existing customer
    let member = u("00000000-0000-4000-8000-0000000d0012"); // a loyalty member
    sqlx::raw_sql(&format!(
        "INSERT INTO organizations (id, name, slug) VALUES ('{org}', 'Org', 'org-w2'), ('{org2}', 'Other', 'org-w2b');
         INSERT INTO branches (id, org_id, name) VALUES ('{branch}', '{org}', 'B'), ('{branch2}', '{org2}', 'B2');
         INSERT INTO users (id, org_id, name, email, password_hash, role) VALUES ('{teller}', '{org}', 'T', 'w2@t.com', 'h', 'teller');
         INSERT INTO customers (id, org_id, name, phone, phone_key, source) VALUES
            ('{known}', '{org}', 'Known Omar', '01001234567', '201001234567', 'pos'),
            ('{member}', '{org}', 'Member Mona', '01112345678', '201112345678', 'loyalty');
         INSERT INTO loyalty_customers (id, org_id, member_token) VALUES ('{member}', '{org}', 'tok-w2-1');"
    ))
    .execute(&db)
    .await
    .expect("seed people");

    let till: Uuid = sqlx::query_scalar("INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1,$2,'open',0) RETURNING id")
        .bind(branch)
        .bind(teller)
        .fetch_one(&db)
        .await
        .unwrap();
    // Delivery orders: (org, branch, name, phone, address, created days ago).
    let deliveries: [(Uuid, Uuid, &str, &str, Option<&str>, i32); 6] = [
        (
            org,
            branch,
            "Omar typed differently",
            "201001234567",
            Some("1 Known St"),
            9,
        ), // → known; name NOT adopted
        (
            org,
            branch,
            "New Nadia",
            "201223334444",
            Some("7 Nadia St., Flat 2"),
            8,
        ), // → created, earliest name
        (
            org,
            branch,
            "Nadia again",
            "201223334444",
            Some("7 nadia st flat 2"),
            3,
        ), // → same customer, same address
        (org, branch, "Bad Phone", "12345", Some("3 Nowhere"), 5), // → no customer
        (
            org2,
            branch2,
            "Other Org Nadia",
            "201223334444",
            Some("9 Elsewhere"),
            4,
        ), // → its own org's customer
        (org, branch, "Pickup Pat", "201009998877", None, 2),      // → customer, no address
    ];
    let mut ids = Vec::new();
    for (i, (o, b, name, phone, addr, days)) in deliveries.iter().enumerate() {
        let channel = if addr.is_some() { "outside" } else { "pickup" };
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO delivery_orders (org_id, branch_id, channel, delivery_ref, customer_name, customer_phone,
                    address_line, subtotal, delivery_fee, total, cart, deductions_snapshot, otp_verified, created_at,
                    tax_amount, tax_rate_applied, tax_inclusive)
             VALUES ($1, $2, $3::delivery_channel, $4, $5, $6, $7, 1000, 0, 1000, '[]', '[]', true, now() - make_interval(days => $8),
                     0, 0, false)
             RETURNING id",
        )
        .bind(o)
        .bind(b)
        .bind(channel)
        .bind(format!("D-W2-{i}"))
        .bind(name)
        .bind(phone)
        .bind(addr)
        .bind(days)
        .fetch_one(&db)
        .await
        .unwrap_or_else(|e| panic!("seed delivery {i}: {e}"));
        ids.push(id);
    }
    // Two sales: one from a delivery, one rung for the loyalty member.
    let sale = |n: i32, extra_col: &'static str, extra: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(&format!(
                "INSERT INTO orders (branch_id, teller_id, till_id, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref, {extra_col}) \
                 VALUES ($1, $2, $3, 1000, 0, 1000, 'completed', $4, 'cash', $5, $6) RETURNING id"
            ))
            .bind(branch)
            .bind(teller)
            .bind(till)
            .bind(n)
            .bind(format!("W2-{n}"))
            .bind(extra)
            .fetch_one(&db)
            .await
            .unwrap_or_else(|e| panic!("seed order {n}: {e}"))
        }
    };
    let sale_delivery = sale(1, "delivery_order_id", ids[1]).await;
    let sale_member = sale(2, "loyalty_customer_id", member).await;
    let booking_at = "now() + interval '2 days'";
    sqlx::raw_sql(&format!(
        "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, guest_phone, source) VALUES
            ('{org}', '{branch}', 2, {booking_at}, {booking_at} + interval '2 hours', 'Booker Basma', '201556667777', 'host'),
            ('{org}', '{branch}', 2, {booking_at} + interval '1 day', {booking_at} + interval '26 hours', 'Nadia books', '201223334444', 'public'),
            ('{org}', '{branch}', 2, {booking_at} + interval '2 days', {booking_at} + interval '50 hours', 'No Phone', 'n/a', 'host');"
    ))
    .execute(&db)
    .await
    .expect("seed bookings");

    subset(|v| v >= REFERENCES)
        .run(&db)
        .await
        .expect("the wave-2 migrations");

    let cust = |key: &'static str, o: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (Uuid, String, String)>(
                "SELECT id, name, source FROM customers WHERE org_id = $1 AND phone_key = $2 AND merged_into IS NULL",
            )
            .bind(o)
            .bind(key)
            .fetch_optional(&db)
            .await
            .unwrap()
        }
    };
    let link = |table: &'static str, col: &'static str, val: String| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>(&format!(
                "SELECT customer_id FROM {table} WHERE {col} = $1"
            ))
            .bind(val)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };

    // An existing customer is matched and keeps their name.
    assert_eq!(
        link(
            "delivery_orders",
            "customer_name",
            "Omar typed differently".into()
        )
        .await,
        Some(known)
    );
    assert_eq!(cust("201001234567", org).await.unwrap().1, "Known Omar");
    // A new phone makes ONE customer, named as on first contact, source online.
    let (nadia, name, source) = cust("201223334444", org).await.expect("nadia");
    assert_eq!((name.as_str(), source.as_str()), ("New Nadia", "online"));
    assert_eq!(
        link("delivery_orders", "customer_name", "Nadia again".into()).await,
        Some(nadia)
    );
    assert_eq!(
        link("bookings", "guest_name", "Nadia books".into()).await,
        Some(nadia),
        "the booking finds the online customer"
    );
    // Tenants do not share people.
    let (nadia2, ..) = cust("201223334444", org2).await.expect("other org's nadia");
    assert_ne!(nadia, nadia2);
    assert_eq!(
        link("delivery_orders", "customer_name", "Other Org Nadia".into()).await,
        Some(nadia2)
    );
    // A phone that fails the rule makes no customer.
    assert_eq!(
        link("delivery_orders", "customer_name", "Bad Phone".into()).await,
        None
    );
    assert_eq!(
        link("bookings", "guest_name", "No Phone".into()).await,
        None
    );
    // A booking-only guest is created with source booking.
    let (basma, _, source) = cust("201556667777", org).await.expect("basma");
    assert_eq!(source, "booking");
    assert_eq!(
        link("bookings", "guest_name", "Booker Basma".into()).await,
        Some(basma)
    );
    // orders.customer_id from the delivery row, and from the loyalty member.
    let of_order = |id: Uuid| {
        let db = db.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>("SELECT customer_id FROM orders WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert_eq!(of_order(sale_delivery).await, Some(nadia));
    assert_eq!(of_order(sale_member).await, Some(member));
    // Addresses: Nadia's two spellings are one address used twice; pickup has none.
    let nadia_addr: Vec<(i32, String)> = sqlx::query_as(
        "SELECT use_count, address_line FROM customer_addresses WHERE customer_id = $1",
    )
    .bind(nadia)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(nadia_addr, vec![(2, "7 Nadia St., Flat 2".to_string())]);
    let (pat, ..) = cust("201009998877", org).await.expect("pat");
    assert_eq!(
        i64_of(
            &db,
            &format!("SELECT count(*) FROM customer_addresses WHERE customer_id = '{pat}'")
        )
        .await,
        0
    );
    assert_eq!(
        i64_of(
            &db,
            "SELECT count(*) FROM delivery_orders WHERE address_id IS NOT NULL"
        )
        .await,
        4
    );
    assert_eq!(
        i64_of(
            &db,
            "SELECT count(*) FROM delivery_orders WHERE contact_override"
        )
        .await,
        0
    );
    // Capability 225 landed.
    assert_eq!(i64_of(&db, "SELECT count(*) FROM capabilities WHERE id = 225 AND key = 'customers.addresses.view' AND defaults = 'omt'").await, 1);

    // Idempotent: running the backfill's statements again links nothing new.
    let before = i64_of(&db, "SELECT count(*) FROM customers").await;
    let sql = std::fs::read_to_string(
        std::path::Path::new(
            &std::env::var("CARGO_MANIFEST_DIR")
                .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string()),
        )
        .join("migrations/20260925070000_customer_references.sql"),
    )
    .unwrap();
    sqlx::raw_sql(&sql)
        .execute(&db)
        .await
        .expect("the references migration re-runs cleanly");
    assert_eq!(i64_of(&db, "SELECT count(*) FROM customers").await, before);
}
