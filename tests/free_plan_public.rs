//! The Free plan's public side: every guest page a Free shop does not have is a
//! 404, the sweeping jobs skip Free orgs, and a shop can sign itself up.
//!
//! The 404 table runs every request on ONE org twice, Paid then Free: it must
//! answer (anything but 404) on Paid and 404 on Free, so a row cannot pass by
//! the seed being wrong, and removing any gate fails its row.

mod common;

use actix_web::{App, http::StatusCode, test, web};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::members::seed_loyalty_member;
use common::shops::{branch, menu_item, pickup, shop};

fn secret() -> madar_rust::auth::jwt::JwtSecret {
    madar_rust::auth::jwt::JwtSecret("secret".to_string())
}

macro_rules! app {
    ($pool:expr) => {{
        unsafe {
            std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
            std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
            std::env::set_var("PUBLIC_SHOP_SUBDOMAINS", "1");
        }
        let read_pool = web::Data::new($pool.clone());
        test::init_service(
            App::new()
                .app_data(web::Data::new($pool.clone()))
                .app_data(web::Data::new(
                    madar_rust::menu::cache::MenuCache::from_env(),
                ))
                .app_data(web::Data::new(secret()))
                .app_data(web::Data::new(
                    madar_rust::auth::org_status::OrgStatusCache::new(),
                ))
                .app_data(web::Data::new(
                    madar_rust::realtime::hub::BranchEventHub::new(),
                ))
                .app_data(madar_rust::qr_card::routes::make_provider())
                .app_data(web::Data::new(
                    madar_rust::demo::config::DemoConfig::from_env(),
                ))
                .app_data(web::Data::new(madar_rust::ai::AiState::from_env()))
                .configure(|cfg| madar_rust::app_routes::configure_api(cfg, read_pool.clone())),
        )
        .await
    }};
}

async fn call<S, B>(app: &S, req: test::TestRequest) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn set_plan(pool: &PgPool, org: Uuid, plan: &str) {
    sqlx::query("UPDATE organizations SET plan = $2 WHERE id = $1")
        .bind(org)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
}

// ── The public pages ──────────────────────────────────────────────────────

struct Guest {
    org: Uuid,
    branch: Uuid,
    table: Uuid,
    member: Uuid,
    member_token: String,
    booking_token: String,
    delivery_order: Uuid,
    item: Uuid,
}

async fn seed_guest_shop(pool: &PgPool) -> Guest {
    let org = shop(pool, "rue", "The Rue").await;
    let branch = branch(pool, org, "Maadi").await;
    pickup(pool, branch).await;
    menu_item(pool, org, "Drinks", "Latte", 5000).await;
    let item: Uuid = sqlx::query_scalar("SELECT id FROM menu_items WHERE org_id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO branch_booking_settings (branch_id, org_id, enabled, require_otp, lead_time_minutes)
         VALUES ($1, $2, true, false, 60)",
    )
    .bind(branch)
    .bind(org)
    .execute(pool)
    .await
    .unwrap();
    let table: Uuid = sqlx::query_scalar(
        "INSERT INTO branch_tables (org_id, branch_id, label, seats) VALUES ($1, $2, 'T1', 4) RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(pool)
    .await
    .unwrap();
    let member_token = "tok-member-1".to_string();
    let member = seed_loyalty_member(pool, org, "+201001234567", "Mona", &member_token).await;
    sqlx::query("UPDATE loyalty_customers SET apple_auth_token = 'apple-secret' WHERE id = $1")
        .bind(member)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO loyalty_pass_devices (device_library_id, customer_id, org_id, push_token) VALUES ('dev-1', $1, $2, 'p')")
        .bind(member)
        .bind(org)
        .execute(pool)
        .await
        .unwrap();
    let booking_token: String = sqlx::query_scalar(
        "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, guest_phone)
         VALUES ($1, $2, 2, now() + interval '3 days', now() + interval '3 days 90 minutes', 'A', '2010')
         RETURNING manage_token",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(pool)
    .await
    .unwrap();
    let delivery_order: Uuid = sqlx::query_scalar(
        "INSERT INTO delivery_orders (org_id, branch_id, channel, delivery_ref, customer_name, customer_phone,
                subtotal, delivery_fee, total, cart, deductions_snapshot, otp_verified, tax_amount, tax_rate_applied, tax_inclusive)
         VALUES ($1, $2, 'pickup'::delivery_channel, 'D-1', 'Ali', '201001234567', 1000, 0, 1000, '[]', '[]', true, 0, 0, false)
         RETURNING id",
    )
    .bind(org)
    .bind(branch)
    .fetch_one(pool)
    .await
    .unwrap();
    Guest {
        org,
        branch,
        table,
        member,
        member_token,
        booking_token,
        delivery_order,
        item,
    }
}

type Row = (&'static str, String, Option<Value>);

fn public_requests(g: &Guest) -> Vec<Row> {
    let (org, br) = (g.org, g.branch);
    let cart = json!({"items": [{"menu_item_id": g.item, "quantity": 1}]});
    vec![
        // online ordering, delivery and the public menu
        ("GET", format!("/public/branches?org_id={org}"), None),
        (
            "GET",
            format!("/public/branches?org_id={org}&browse=true"),
            None,
        ),
        (
            "GET",
            format!("/public/branches/{br}/menu?channel=pickup"),
            None,
        ),
        (
            "GET",
            format!("/public/branches/{br}/menu?channel=dine_in&preview=true"),
            None,
        ),
        (
            "POST",
            format!("/public/branches/{br}/cart-quote"),
            Some(cart.clone()),
        ),
        (
            "GET",
            format!("/public/branches/{br}/delivery-quote?lat=30.0&lng=31.0&channel=pickup"),
            None,
        ),
        (
            "GET",
            format!("/public/delivery-orders/{}/track", g.delivery_order),
            None,
        ),
        (
            "POST",
            "/public/delivery-orders".into(),
            Some(json!({
            "branch_id": br, "channel": "pickup", "customer_name": "Ali",
            "customer_phone": "01001234567", "payment_method_hint": "cash", "device_token": "x",
            "items": [{"menu_item_id": g.item, "quantity": 1}]})),
        ),
        (
            "GET",
            format!(
                "/public/delivery-orders/history?phone=01001234567&org_id={org}&device_token=x"
            ),
            None,
        ),
        (
            "GET",
            format!(
                "/public/delivery-orders/past-locations?phone=01001234567&org_id={org}&device_token=x"
            ),
            None,
        ),
        ("GET", format!("/public/order-now/{}", g.member_token), None),
        // the shop's own page
        ("GET", "/public/orgs/brand?slug=rue".into(), None),
        ("GET", "/public/orgs/links?slug=rue".into(), None),
        ("GET", format!("/public/orgs/favicon?org_id={org}"), None),
        (
            "GET",
            "/public/tenant-shell?host=rue.madar-pos.cloud&path=/".into(),
            None,
        ),
        // loyalty and the wallet
        (
            "GET",
            format!("/public/loyalty/join-info?org_id={org}"),
            None,
        ),
        (
            "GET",
            format!("/public/loyalty/join-info?branch_id={br}"),
            None,
        ),
        (
            "POST",
            "/public/loyalty/join".into(),
            Some(json!({"org_id": org, "name": "Sara", "phone": "01009998877"})),
        ),
        (
            "GET",
            format!("/public/loyalty/card/{}", g.member_token),
            None,
        ),
        (
            "GET",
            format!("/public/loyalty/card/{}/orders", g.member_token),
            None,
        ),
        (
            "POST",
            format!("/public/loyalty/card/{}/preferences", g.member_token),
            Some(json!({"locale": "en"})),
        ),
        (
            "GET",
            format!("/public/loyalty/card/{}/qr.png", g.member_token),
            None,
        ),
        (
            "GET",
            format!("/public/loyalty/pass/{}/apple.pkpass", g.member_token),
            None,
        ),
        (
            "GET",
            format!("/public/loyalty/brand/{org}/logo/1.png"),
            None,
        ),
        (
            "GET",
            format!("/public/loyalty/brand/{org}/banner/1.png"),
            None,
        ),
        (
            "GET",
            format!("/wallet/v1/passes/pass.test/{}", g.member),
            None,
        ),
        // bookings
        (
            "GET",
            format!("/public/booking-branches?org_id={org}"),
            None,
        ),
        ("GET", format!("/public/branches/{br}/booking-info"), None),
        (
            "GET",
            format!(
                "/public/branches/{br}/booking-slots?date={}&party_size=2",
                (Utc::now() + Duration::days(3)).format("%Y-%m-%d")
            ),
            None,
        ),
        (
            "POST",
            "/public/bookings".into(),
            Some(json!({
            "branch_id": br, "starts_at": Utc::now() + Duration::days(3), "party_size": 2,
            "guest_name": "A", "phone": "01001234567"})),
        ),
        ("GET", format!("/public/bookings/{}", g.booking_token), None),
        (
            "PATCH",
            format!("/public/bookings/{}", g.booking_token),
            Some(json!({"notes": "x"})),
        ),
        (
            "POST",
            format!("/public/bookings/{}/cancel", g.booking_token),
            None,
        ),
        // a table's QR
        ("GET", format!("/public/tables/{}", g.table), None),
        ("GET", format!("/public/tables/{}/menu", g.table), None),
        (
            "POST",
            format!("/public/tables/{}/cart-quote", g.table),
            Some(cart),
        ),
        (
            "POST",
            "/public/table-orders".into(),
            Some(json!({"table_id": g.table, "items": [{"menu_item_id": g.item, "quantity": 1}]})),
        ),
    ]
}

/// The plan gate's own 404 (a page that does not exist), not a handler's
/// "no logo" or "no such booking".
fn gated(st: StatusCode, body: &Value) -> bool {
    st == StatusCode::NOT_FOUND && (body.is_null() || body["error"] == "Not found: Not found")
}

fn request(row: &Row) -> test::TestRequest {
    let (method, uri, body) = row;
    let mut r = test::TestRequest::default()
        .method(actix_web::http::Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri);
    if uri.starts_with("/wallet/v1") {
        r = r.insert_header(("Authorization", "ApplePass apple-secret"));
    }
    match body {
        Some(b) => r.set_json(b),
        None => r,
    }
}

#[sqlx::test]
async fn every_public_page_of_a_free_shop_is_a_404_and_a_paid_shop_keeps_them(pool: PgPool) {
    let g = seed_guest_shop(&pool).await;
    let app = app!(pool);
    let rows = public_requests(&g);

    // Paid: every one of them reaches its handler.
    let mut paid = Vec::new();
    for row in &rows {
        let (st, body) = call(&app, request(row)).await;
        assert!(
            !gated(st, &body),
            "PAID {} {} must exist: {st} {body}",
            row.0,
            row.1
        );
        paid.push(st);
    }
    // A signed-up Free shop has none of them but a table's QR (the floor is
    // on Free). The gate is the only difference.
    set_plan(&pool, g.org, "free").await;
    for row in &rows {
        let (st, body) = call(&app, request(row)).await;
        if row.1.starts_with("/public/tables/") || row.1 == "/public/table-orders" {
            assert!(
                !gated(st, &body),
                "FREE {} {} is the floor, it must exist: {st} {body}",
                row.0,
                row.1
            );
            continue;
        }
        assert!(
            gated(st, &body),
            "FREE {} {} must be the gate's 404: {st} {body}",
            row.0,
            row.1
        );
        assert!(
            body.get("code").is_none_or(|c| c != "plan_feature"),
            "404, never a plan refusal: {body}"
        );
    }
    // By id the brand is the shop's own dashboard reading its name and logo:
    // a Free shop keeps it (only a web address, by slug, is gone).
    let (st, body) = call(
        &app,
        test::TestRequest::get().uri(&format!("/public/orgs/brand?org_id={}", g.org)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "free: its own brand by id: {body}");
    // The wallet's list of passes leaves a Free shop's passes out.
    let (st, _) = call(
        &app,
        test::TestRequest::get().uri("/wallet/v1/devices/dev-1/registrations/pass.test"),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "free: nothing to update");
    set_plan(&pool, g.org, "paid").await;
    let (st, body) = call(
        &app,
        test::TestRequest::get().uri("/wallet/v1/devices/dev-1/registrations/pass.test"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "paid lists its passes: {body}");
    assert_eq!(body["serialNumbers"], json!([g.member.to_string()]));
    let _ = paid;
}

#[sqlx::test]
async fn the_signup_otp_and_the_delivery_otp_are_not_behind_any_shop_s_plan(pool: PgPool) {
    let g = seed_guest_shop(&pool).await;
    set_plan(&pool, g.org, "free").await;
    let app = app!(pool);
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": "01005550001"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    // The delivery OTP names no shop at all: a phone proof, shared by every shop.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/otp/request")
            .set_json(json!({"phone": "01005550002"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

// ── Jobs ─────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn the_delivery_sweep_skips_a_free_org(pool: PgPool) {
    let g = seed_guest_shop(&pool).await;
    sqlx::query("UPDATE branch_delivery_settings SET auto_reject_minutes = 1 WHERE branch_id = $1")
        .bind(g.branch)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE delivery_orders SET status = 'received', created_at = now() - interval '2 hours'",
    )
    .execute(&pool)
    .await
    .unwrap();
    set_plan(&pool, g.org, "free").await;
    assert!(
        madar_rust::delivery::jobs::reject_unaccepted(&pool)
            .await
            .unwrap()
            .is_empty()
    );
    set_plan(&pool, g.org, "paid").await;
    assert_eq!(
        madar_rust::delivery::jobs::reject_unaccepted(&pool)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test]
async fn the_bookings_sweep_skips_a_free_org(pool: PgPool) {
    let g = seed_guest_shop(&pool).await;
    sqlx::query("UPDATE branch_booking_settings SET reminder_lead_minutes = 60, auto_no_show_minutes = 15 WHERE branch_id = $1")
        .bind(g.branch).execute(&pool).await.unwrap();
    // A booking that wants a reminder and an arrival ping now, and one long past.
    sqlx::query("UPDATE bookings SET created_at = now() - interval '10 days', starts_at = now() + interval '10 minutes', ends_at = now() + interval '100 minutes'")
        .execute(&pool).await.unwrap();
    let old: Uuid = sqlx::query_scalar(
        "INSERT INTO bookings (org_id, branch_id, party_size, starts_at, ends_at, guest_name, guest_phone)
         VALUES ($1, $2, 2, now() - interval '5 hours', now() - interval '3 hours', 'Old', '2011') RETURNING id",
    ).bind(g.org).bind(g.branch).fetch_one(&pool).await.unwrap();
    let hub = madar_rust::realtime::hub::BranchEventHub::new();
    set_plan(&pool, g.org, "free").await;
    madar_rust::bookings::jobs::run_tick(&pool, &hub)
        .await
        .unwrap();
    let (reminded, arrived): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE reminder_sent_at IS NOT NULL), count(*) FILTER (WHERE arriving_notified_at IS NOT NULL) FROM bookings",
    ).fetch_one(&pool).await.unwrap();
    assert_eq!(
        (reminded, arrived),
        (0, 0),
        "a Free org gets no reminders or arrival pings"
    );
    let status: String = sqlx::query_scalar("SELECT status::text FROM bookings WHERE id = $1")
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "confirmed", "nor is its booking rolled to no_show");
    set_plan(&pool, g.org, "paid").await;
    madar_rust::bookings::jobs::run_tick(&pool, &hub)
        .await
        .unwrap();
    let (reminded, arrived): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE reminder_sent_at IS NOT NULL), count(*) FILTER (WHERE arriving_notified_at IS NOT NULL) FROM bookings",
    ).fetch_one(&pool).await.unwrap();
    assert!(
        reminded >= 1 && arrived >= 1,
        "a paid org gets both: {reminded} {arrived}"
    );
    let status: String = sqlx::query_scalar("SELECT status::text FROM bookings WHERE id = $1")
        .bind(old)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "no_show");
}

#[sqlx::test]
async fn the_loyalty_jobs_skip_a_free_org(pool: PgPool) {
    let g = seed_guest_shop(&pool).await;
    sqlx::query(
        "INSERT INTO loyalty_settings (org_id, enabled, program_name, mode, earn_piastres_per_point,
             default_reward_cost, winback_enabled, winback_reward_amount, birthday_enabled)
         VALUES ($1, true, 'Rue Rewards', 'points', 1000, 100, true, 5, true)",
    ).bind(g.org).execute(&pool).await.unwrap();
    // The member last visited ten days ago, was born today, and holds a pass the
    // wallet has never refreshed.
    sqlx::query(
        "INSERT INTO loyalty_transactions (org_id, customer_id, branch_id, kind, currency, points, source, created_at)
         VALUES ($1,$2,$3,'adjust','points',1,'manual', now() - interval '10 days')",
    ).bind(g.org).bind(g.member).bind(g.branch).execute(&pool).await.unwrap();
    // Born "today" in the ORG's zone, which is the date the job reads: a UTC
    // date is already tomorrow in Cairo from 21:00 UTC (and failed nightly).
    sqlx::query("UPDATE customers c SET birth_month = EXTRACT(MONTH FROM (now() AT TIME ZONE o.timezone::text))::smallint, \
                        birth_day = EXTRACT(DAY FROM (now() AT TIME ZONE o.timezone::text))::smallint \
                   FROM organizations o WHERE o.id = c.org_id AND c.id = $1")
        .bind(g.member).execute(&pool).await.unwrap();
    sqlx::query("UPDATE loyalty_customers SET google_object_id = 'g-1', pass_updated_at = NULL WHERE id = $1")
        .bind(g.member).execute(&pool).await.unwrap();

    for (plan, expected) in [("free", false), ("paid", true)] {
        set_plan(&pool, g.org, plan).await;
        let due = madar_rust::loyalty::birthdays::due_greetings(&pool, None)
            .await
            .unwrap();
        assert_eq!(!due.is_empty(), expected, "{plan}: birthdays");
        let stale = madar_rust::loyalty::wallet::refresh::stale_passes(&pool, 100)
            .await
            .unwrap();
        assert_eq!(!stale.is_empty(), expected, "{plan}: wallet refresh");
        sqlx::query("DELETE FROM loyalty_winbacks")
            .execute(&pool)
            .await
            .unwrap();
        madar_rust::loyalty::winback::run_tick(&pool).await.unwrap();
        let sent: i64 = sqlx::query_scalar("SELECT count(*) FROM loyalty_winbacks")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sent > 0, expected, "{plan}: winback");
    }
}

#[sqlx::test]
async fn the_dawam_jobs_skip_a_free_org(pool: PgPool) {
    let g = seed_guest_shop(&pool).await;
    sqlx::query("UPDATE organizations SET modules = '{pos,dawam}' WHERE id = $1")
        .bind(g.org)
        .execute(&pool)
        .await
        .unwrap();
    set_plan(&pool, g.org, "free").await;
    madar_rust::staff::jobs::open_pay_periods(&pool)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM payroll_periods WHERE org_id = $1")
        .bind(g.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "a Free org's pay period does not open itself");
    set_plan(&pool, g.org, "paid").await;
    madar_rust::staff::jobs::open_pay_periods(&pool)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM payroll_periods WHERE org_id = $1")
        .bind(g.org)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(n >= 1, "a paid Dawam org's does");
}

// ── Signup ───────────────────────────────────────────────────────────────

fn signup_body(phone: &str, code: &str, email: &str) -> Value {
    json!({
        "phone": phone, "code": code, "shop_name": "Mona's Coffee", "owner_name": "Mona",
        "email": email, "password": "correct horse 1", "template": "cafe", "terms_version": "2026-10"
    })
}

async fn latest_code(pool: &PgPool, phone: &str) -> String {
    sqlx::query_scalar(
        "SELECT code FROM delivery_otp WHERE phone = $1 AND purpose = 'signup' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(madar_rust::phone::canonical(phone).unwrap())
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_shop_signs_itself_up_on_the_free_plan_and_is_signed_straight_in(pool: PgPool) {
    let app = app!(pool);
    let phone = "01001112233";
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    assert_eq!(b["sent"], true);
    let code = latest_code(&pool, phone).await;
    assert_eq!(code.len(), 6);
    assert!(code.chars().all(|c| c.is_ascii_digit()));

    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, &code, "Mona@Coffee.test")),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    // The session is the shape /auth/login answers with.
    let token = b["token"].as_str().expect("a session token").to_string();
    assert_eq!(b["user"]["role"], "org_admin");
    assert_eq!(b["user"]["email"], "mona@coffee.test");
    assert_eq!(b["plan"], "free");
    assert_eq!(b["currency_code"], "EGP");
    assert!(b["tax_policy"].is_object());
    assert!(b["open_till"].is_null());
    let org: Uuid = b["org_id"].as_str().unwrap().parse().unwrap();
    let branch: Uuid = b["branch_id"].as_str().unwrap().parse().unwrap();

    let (plan, source, modules, terms, slug, terms_at): (String, String, Vec<String>, Option<String>, Option<String>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT plan, signup_source, modules, signup_terms_version, slug, signup_terms_accepted_at FROM organizations WHERE id = $1")
            .bind(org).fetch_one(&pool).await.unwrap();
    assert_eq!((plan.as_str(), source.as_str()), ("free", "self"));
    assert_eq!(modules, vec!["pos".to_string()]);
    assert_eq!(terms.as_deref(), Some("2026-10"));
    assert!(terms_at.is_some());
    let slug = slug.unwrap();
    let (name_part, tail) = slug.rsplit_once('-').unwrap();
    assert_eq!(name_part, "mona-s-coffee");
    assert_eq!(tail.len(), 4);
    let (bname, n_branches): (String, i64) = sqlx::query_as(
        "SELECT (SELECT name FROM branches WHERE id = $1), (SELECT count(*) FROM branches WHERE org_id = $2)",
    ).bind(branch).bind(org).fetch_one(&pool).await.unwrap();
    assert_eq!((bname.as_str(), n_branches), ("Mona's Coffee", 1));
    let owner_phone: String =
        sqlx::query_scalar("SELECT phone FROM users WHERE org_id = $1 AND role = 'org_admin'")
            .bind(org)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(owner_phone, madar_rust::phone::canonical(phone).unwrap());

    // The token works, and the owner is cut to the Free ceiling.
    let (st, me) = call(
        &app,
        test::TestRequest::get()
            .uri("/auth/me")
            .insert_header(("Authorization", format!("Bearer {token}"))),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{me}");
    assert_eq!(me["plan"], "free");
    let owner: Uuid = b["user"]["id"].as_str().unwrap().parse().unwrap();
    let eff = madar_rust::authz::require::effective(&pool, owner, None)
        .await
        .unwrap();
    assert!(eff.can(madar_rust::authz::Cap::OrdersCreate));
    assert!(!eff.can(madar_rust::authz::Cap::InventoryRead));
    // ...can sign in the ordinary way too, and `plan` comes back.
    let (st, login) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({"email": "mona@coffee.test", "password": "correct horse 1"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{login}");
    assert_eq!(login["plan"], "free");
    // The owner can read the plan, and a Free org cannot add a branch.
    let (st, plan_state) = call(
        &app,
        test::TestRequest::get()
            .uri(&format!("/orgs/{org}/plan"))
            .insert_header(("Authorization", format!("Bearer {token}"))),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{plan_state}");
    assert_eq!(plan_state["plan"], "free");
    assert_eq!(plan_state["signup_source"], "self");
    assert_eq!(plan_state["over_branch_limit"], false);
    let (st, nb) = call(
        &app,
        test::TestRequest::post()
            .uri("/branches")
            .insert_header(("Authorization", format!("Bearer {token}")))
            .set_json(json!({"org_id": org, "name": "Second"})),
    )
    .await;
    assert_eq!(
        (st, nb["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("plan_branch_limit")),
        "{nb}"
    );

    // The code is spent: the same one does nothing twice.
    let (st, again) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body("01009990000", &code, "other@coffee.test")),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{again}");
    // Nor can the phone ask for another code: a shop owns it now.
    let (st, again) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{again}");
    assert_eq!(again["code"], "SIGNUP_PHONE_TAKEN");
    assert!(again["error"].as_str().unwrap().contains("Sign in instead"));
}

#[sqlx::test]
async fn a_wrong_code_a_spent_attempt_budget_and_a_borrowed_code_make_no_shop(pool: PgPool) {
    let app = app!(pool);
    let phone = "01002223344";
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code = latest_code(&pool, phone).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for i in 0..5 {
        let (st, b) = call(
            &app,
            test::TestRequest::post()
                .uri("/public/signup")
                .set_json(signup_body(phone, wrong, "x@y.test")),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "attempt {i}: {b}");
        assert_eq!(b["code"], "SIGNUP_CODE_WRONG", "{b}");
    }
    // The code allowed five tries; the right one is no good now.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, &code, "x@y.test")),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert!(
        b["error"].as_str().unwrap().contains("Too many attempts"),
        "{b}"
    );
    let orgs: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orgs, 0, "nothing was created");

    // A code that proves a phone to a CUSTOMER flow does not sign a shop up.
    let other = "01006667788";
    sqlx::query("INSERT INTO delivery_otp (phone, code, expires_at) VALUES ($1, '123456', now() + interval '5 minutes')")
        .bind(madar_rust::phone::canonical(other).unwrap()).execute(&pool).await.unwrap();
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(other, "123456", "z@y.test")),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    // And an expired signup code is no code.
    sqlx::query("INSERT INTO delivery_otp (phone, code, purpose, expires_at) VALUES ($1, '654321', 'signup', now() - interval '1 minute')")
        .bind(madar_rust::phone::canonical(other).unwrap()).execute(&pool).await.unwrap();
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(other, "654321", "z@y.test")),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    let orgs: i64 = sqlx::query_scalar("SELECT count(*) FROM organizations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orgs, 0);
}

#[sqlx::test]
async fn a_duplicate_email_or_phone_is_a_409_with_a_sign_in_hint_and_keeps_the_code(pool: PgPool) {
    let app = app!(pool);
    // A shop that exists already (the super admin made it).
    let existing = shop(&pool, "old-shop", "Old").await;
    sqlx::query("INSERT INTO users (org_id, name, email, password_hash, role, phone) VALUES ($1, 'Owner', 'owner@old.test', 'h', 'org_admin', $2)")
        .bind(existing).bind(madar_rust::phone::canonical("01003334455").unwrap()).execute(&pool).await.unwrap();

    // Its phone cannot even ask for a code.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": "01003334455"})),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("SIGNUP_PHONE_TAKEN")),
        "{b}"
    );
    // Another phone, the same email.
    let phone = "01004445566";
    call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    let code = latest_code(&pool, phone).await;
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, &code, "Owner@Old.test")),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("SIGNUP_EMAIL_TAKEN")),
        "{b}"
    );
    assert!(b["error"].as_str().unwrap().contains("Sign in instead"));
    // The code survived the refusal: a corrected email signs up with it.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, &code, "new@mona.test")),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    // Even a signup that raced past the OTP check is stopped by the phone check.
    sqlx::query("INSERT INTO delivery_otp (phone, code, purpose, expires_at) VALUES ($1, '222222', 'signup', now() + interval '5 minutes')")
        .bind(madar_rust::phone::canonical(phone).unwrap()).execute(&pool).await.unwrap();
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, "222222", "third@mona.test")),
    )
    .await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::CONFLICT, Some("SIGNUP_PHONE_TAKEN")),
        "{b}"
    );
}

#[sqlx::test]
async fn signup_fields_are_checked_before_a_code_is_spent(pool: PgPool) {
    let app = app!(pool);
    let phone = "01007778899";
    call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    let code = latest_code(&pool, phone).await;
    for (what, mutate) in [
        ("template", json!({"template": "bakery"})),
        ("password", json!({"password": "short"})),
        ("email", json!({"email": "not-an-email"})),
        ("terms", json!({"terms_version": ""})),
        ("shop name", json!({"shop_name": " "})),
        ("phone", json!({"phone": "123"})),
    ] {
        let mut body = signup_body(phone, &code, "ok@mona.test");
        for (k, v) in mutate.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (st, b) = call(
            &app,
            test::TestRequest::post()
                .uri("/public/signup")
                .set_json(body),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{what}: {b}");
    }
    let attempts: i32 =
        sqlx::query_scalar("SELECT attempts FROM delivery_otp WHERE purpose = 'signup'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0, "a typo costs no attempt at the code");
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, &code, "ok@mona.test")),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
}

#[sqlx::test]
async fn the_signup_otp_is_limited_per_phone_and_per_address(pool: PgPool) {
    let app = app!(pool);
    let ask = |phone: &str| {
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone}))
    };
    let (st, _) = call(&app, ask("01008880001")).await;
    assert_eq!(st, StatusCode::OK);
    // A second code inside the minute.
    let (st, b) = call(&app, ask("01008880001")).await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("SIGNUP_OTP_COOLDOWN")),
        "{b}"
    );
    // Five in the hour.
    let p = madar_rust::phone::canonical("01008880002").unwrap();
    for i in 0..5 {
        sqlx::query("INSERT INTO delivery_otp (phone, code, purpose, expires_at, created_at) VALUES ($1, '111111', 'signup', now() + interval '5 minutes', now() - make_interval(mins => $2::int))")
            .bind(&p).bind(5 + i * 10).execute(&pool).await.unwrap();
    }
    let (st, b) = call(&app, ask("01008880002")).await;
    assert_eq!(
        (st, b["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("SIGNUP_OTP_LIMIT")),
        "{b}"
    );
    // Another phone is unaffected.
    let (st, _) = call(&app, ask("01008880003")).await;
    assert_eq!(st, StatusCode::OK);
}

#[sqlx::test]
async fn the_signup_routes_have_a_per_address_limit(pool: PgPool) {
    // The limiter ON, as in production (the other tests switch it off).
    unsafe {
        std::env::remove_var("MADAR_DISABLE_RATE_LIMIT");
        std::env::set_var("MADAR_RL_SIGNUP_OTP_BURST", "3");
        std::env::set_var("MADAR_RL_SIGNUP_OTP_MS_PER_REQUEST", "60000");
        std::env::set_var("MADAR_RL_SIGNUP_CREATE_BURST", "2");
        std::env::set_var("MADAR_RL_SIGNUP_CREATE_MS_PER_REQUEST", "60000");
    }
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(pool.clone()))
            .app_data(web::Data::new(secret()))
            .configure(madar_rust::plan::signup::configure),
    )
    .await;
    let addr = "10.1.2.3:4000".parse::<std::net::SocketAddr>().unwrap();
    let mut seen = Vec::new();
    for i in 0..5 {
        let r = test::TestRequest::post()
            .uri("/public/signup/otp")
            .peer_addr(addr)
            .set_json(json!({"phone": format!("010099900{i:02}")}))
            .to_request();
        seen.push(test::call_service(&app, r).await.status());
    }
    assert_eq!(&seen[..3], &[StatusCode::OK; 3]);
    assert_eq!(
        &seen[3..],
        &[StatusCode::TOO_MANY_REQUESTS; 2],
        "the fourth from one address is limited: {seen:?}"
    );
    // Another address has its own allowance.
    let r = test::TestRequest::post()
        .uri("/public/signup/otp")
        .peer_addr("10.9.9.9:1".parse().unwrap())
        .set_json(json!({"phone": "01009990099"}))
        .to_request();
    assert_eq!(test::call_service(&app, r).await.status(), StatusCode::OK);
    // And creating a shop has its own bucket.
    let mut created = Vec::new();
    for _ in 0..3 {
        let r = test::TestRequest::post()
            .uri("/public/signup")
            .peer_addr(addr)
            .set_json(signup_body("01009990000", "000000", "a@b.test"))
            .to_request();
        created.push(test::call_service(&app, r).await.status());
    }
    assert_eq!(created[2], StatusCode::TOO_MANY_REQUESTS, "{created:?}");
    assert_ne!(created[0], StatusCode::TOO_MANY_REQUESTS);
}

/// The path a shop that signed itself up walks first, as its owner, with no
/// super admin anywhere: onboarding saves the name and currency, then the team.
/// Both were refused on the first release (the org update was super-admin
/// only; the new-account guard compared the owner's Free-cut set against a
/// role's paid defaults).
#[sqlx::test]
async fn a_signed_up_owner_names_the_shop_and_adds_the_team(pool: PgPool) {
    let app = app!(pool);
    let phone = "01004445566";
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let code = latest_code(&pool, phone).await;
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(signup_body(phone, &code, "owner@shop.test")),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    let token = b["token"].as_str().unwrap().to_string();
    let org: Uuid = b["org_id"].as_str().unwrap().parse().unwrap();
    let branch: Uuid = b["branch_id"].as_str().unwrap().parse().unwrap();
    let owner: Uuid = b["user"]["id"].as_str().unwrap().parse().unwrap();
    let auth = ("Authorization", format!("Bearer {token}"));

    // Onboarding's first step: the name and the currency.
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{org}"))
            .insert_header(auth.clone())
            .set_json(json!({"name": "Bean There", "currency_code": "USD"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "the owner saves the name and currency: {b}"
    );
    assert_eq!(b["name"], "Bean There");
    assert_eq!(b["currency_code"], "USD");

    // What stays the super admin's: the address, the plan, the modules.
    for body in [
        json!({"slug": "bean-there"}),
        json!({"plan": "paid"}),
        json!({"modules": ["pos", "inventory"]}),
    ] {
        let (st, b) = call(
            &app,
            test::TestRequest::patch()
                .uri(&format!("/orgs/{org}"))
                .insert_header(auth.clone())
                .set_json(&body),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{body}: {b}");
        assert_eq!(b["code"], "ORG_FIELD_MADAR_ONLY", "says to ask Madar: {b}");
    }

    // The team: every role an owner can give on Free (a waiter too: Free has the floor).
    let mut teller = Uuid::nil();
    for (i, role) in ["teller", "branch_manager", "org_admin"].iter().enumerate() {
        let (st, b) = call(
            &app,
            test::TestRequest::post()
                .uri("/users")
                .insert_header(auth.clone())
                .set_json(json!({
                    "org_id": org, "name": format!("Staff {i}"), "role": role,
                    "pin": format!("39{i}172"),
                    "email": format!("staff{i}@shop.test"), "password": "correct horse 2",
                })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "the owner adds a {role}: {b}");
        if *role == "teller" {
            teller = b["user"]["id"].as_str().unwrap().parse().unwrap();
        }
    }

    // ...and moves a person to another role (the Users page's role picker).
    let (st, roles) = call(
        &app,
        test::TestRequest::get()
            .uri("/authz/roles")
            .insert_header(auth.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{roles}");
    let manager = roles
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "branch_manager")
        .expect("a manager role")["id"]
        .clone();
    let (st, b) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/authz/users/{teller}/assignments"))
            .insert_header(auth.clone())
            .set_json(json!({"assignments": [{"role_id": manager, "all_branches": true}]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the owner promotes the teller: {b}");

    // Once the shop has taken an order its currency is the super admin's.
    let till: Uuid = sqlx::query_scalar(
        "INSERT INTO tills (branch_id, teller_id, status, opening_cash) VALUES ($1, $2, 'open', 0) RETURNING id",
    )
    .bind(branch)
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO orders (branch_id, teller_id, till_id, subtotal, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, 100, 0, 100, 'completed', 1, 'cash', $4)",
    )
    .bind(branch)
    .bind(owner)
    .bind(till)
    .bind(format!("R-{}", Uuid::new_v4()))
    .execute(&pool)
    .await
    .unwrap();
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{org}"))
            .insert_header(auth.clone())
            .set_json(json!({"currency_code": "EGP"})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CONFLICT,
        "no currency change after an order: {b}"
    );
    assert_eq!(b["code"], "ORG_CURRENCY_LOCKED", "{b}");
    // The same currency again, and a new name, are still fine.
    let (st, b) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{org}"))
            .insert_header(auth.clone())
            .set_json(json!({"name": "Bean There Too", "currency_code": "USD"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}

async fn signup_owner<S, B>(app: &S, pool: &PgPool, phone: &str, email: &str) -> (String, Uuid)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let (st, b) = call(
        app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let code = latest_code(pool, phone).await;
    let (st, b) = call(
        app,
        test::TestRequest::post()
            .uri("/public/signup/verify")
            .set_json(json!({"phone": phone, "code": code})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let token = b["signup_token"].as_str().unwrap().to_string();
    let mut body = signup_body(phone, "", email);
    let o = body.as_object_mut().unwrap();
    o.remove("phone");
    o.remove("code");
    o.insert("signup_token".into(), json!(token));
    let (st, b) = call(
        app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(body),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    (
        b["token"].as_str().unwrap().to_string(),
        b["org_id"].as_str().unwrap().parse().unwrap(),
    )
}

/// The code is checked by the SERVER at the code step: a wrong code never gets
/// past it, a right one gives a single-use token, and only that token makes
/// the shop (the phone is the token's, not the body's).
#[sqlx::test]
async fn the_code_is_checked_at_the_code_step_and_its_token_makes_one_shop(pool: PgPool) {
    let app = app!(pool);
    let phone = "01007778899";
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": phone})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code = latest_code(&pool, phone).await;
    let wrong = if code == "000000" { "111111" } else { "000000" };

    // A wrong code: refused right there, with a code the page words.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/verify")
            .set_json(json!({"phone": phone, "code": wrong})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["code"], "SIGNUP_CODE_WRONG", "{b}");
    assert!(b.get("signup_token").is_none());

    // The right one: a token, and nothing made yet.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/verify")
            .set_json(json!({"phone": phone, "code": code})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let token = b["signup_token"].as_str().unwrap().to_string();
    assert!(token.len() >= 64, "unguessable: {token}");
    assert_eq!(b["expires_in_seconds"], 1800);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM organizations WHERE signup_source = 'self'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(n, 0, "verifying makes no shop");
    // Only the token's hash is kept.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT proof_hash FROM delivery_otp WHERE purpose = 'signup' AND verified_at IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stored.is_some_and(|h| h != token && h.len() == 64));

    let body = |t: &str| {
        let mut v = signup_body("01000000000", "", "tok@shop.test");
        let o = v.as_object_mut().unwrap();
        o.remove("code");
        o.insert("signup_token".into(), json!(t));
        v
    };
    // A made-up token: refused.
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(body("deadbeef")),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["code"], "SIGNUP_TOKEN_EXPIRED");

    // The real one makes the shop, on the TOKEN's phone (the body's is ignored).
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(body(&token)),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");
    assert_eq!(
        b["user"]["phone"],
        madar_rust::phone::canonical(phone).unwrap()
    );

    // Once: the same token again makes nothing.
    let mut again = body(&token);
    again["email"] = json!("tok2@shop.test");
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(again),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["code"], "SIGNUP_TOKEN_EXPIRED");

    // A token older than 30 minutes is spent too.
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/otp")
            .set_json(json!({"phone": "01007778800"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let code = latest_code(&pool, "01007778800").await;
    let (_, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup/verify")
            .set_json(json!({"phone": "01007778800", "code": code})),
    )
    .await;
    let old = b["signup_token"].as_str().unwrap().to_string();
    sqlx::query("UPDATE delivery_otp SET verified_at = now() - interval '31 minutes' WHERE verified_at IS NOT NULL AND consumed_at IS NULL")
        .execute(&pool).await.unwrap();
    let mut late = body(&old);
    late["email"] = json!("late@shop.test");
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/public/signup")
            .set_json(late),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["code"], "SIGNUP_TOKEN_EXPIRED");
}

/// What Free has no use for is refused by the server however the request is
/// made, and a paid capability is refused AS the plan's (so the dashboard can
/// say "on the paid plan", not "no permission").
#[sqlx::test]
async fn free_refuses_partner_sharing_and_paid_capabilities_by_plan(pool: PgPool) {
    let app = app!(pool);
    let (token, org) = signup_owner(&app, &pool, "01006665544", "plan@shop.test").await;
    let auth = ("Authorization", format!("Bearer {token}"));

    // A waiter account is fine: Free has the floor (its devices are refused
    // at activation, see tests/free_plan.rs).
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/users")
            .insert_header(auth.clone())
            .set_json(json!({"org_id": org, "name": "W", "role": "waiter", "pin": "482917"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{b}");

    // Sharing a payment method with partners: not on create, not on update.
    let (st, b) = call(&app, test::TestRequest::post().uri("/payment-methods").insert_header(auth.clone())
        .set_json(json!({"name": "wallet", "label_translations": {"en": "Wallet", "ar": "محفظة"}, "color": "#0f766e", "icon": "wallet", "is_cash": false, "visible_in_integrations": false}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_feature", "{b}");
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri("/payment-methods")
            .insert_header(auth.clone())
            .set_json(
                json!({"name": "wallet", "label_translations": {"en": "Wallet", "ar": "محفظة"}, "color": "#0f766e", "icon": "wallet", "is_cash": false}),
            ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "a method itself is fine: {b}");
    let id = b["id"].as_str().unwrap().to_string();
    let (st, b) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/payment-methods/{id}"))
            .insert_header(auth.clone())
            .set_json(json!({"visible_in_integrations": false})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_feature", "{b}");

    // The Talabat tenders stay off on Free (a delivery channel).
    let talabat: String = sqlx::query_scalar(
        "SELECT id::text FROM org_payment_methods WHERE org_id = $1 AND name = 'talabat_online'",
    )
    .bind(org)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (st, b) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/payment-methods/{talabat}"))
            .insert_header(auth.clone())
            .set_json(json!({"is_active": true})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_feature", "{b}");
    assert_eq!(b["feature"], "online", "{b}");
    let (st, b) = call(
        &app,
        test::TestRequest::post()
            .uri(&format!("/payment-methods/{talabat}/activate"))
            .insert_header(auth.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "the activate switch too: {b}");
    assert_eq!(b["code"], "plan_feature", "{b}");

    // A paid capability (inventory) refused as the plan's, not as "no permission".
    let (st, b) = call(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org}/categories"))
            .insert_header(auth.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["code"], "plan_feature", "{b}");
    assert_eq!(b["feature"], "inventory", "{b}");
    assert!(b["error_ar"].is_string(), "{b}");

    // Paid: the rest are fine too.
    set_plan(&pool, org, "paid").await;
    let (st, b) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/payment-methods/{id}"))
            .insert_header(auth.clone())
            .set_json(json!({"visible_in_integrations": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
    let (st, b) = call(
        &app,
        test::TestRequest::put()
            .uri(&format!("/payment-methods/{talabat}"))
            .insert_header(auth.clone())
            .set_json(json!({"is_active": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "paid turns Talabat on: {b}");
    let (st, b) = call(
        &app,
        test::TestRequest::get()
            .uri(&format!("/inventory/orgs/{org}/categories"))
            .insert_header(auth.clone()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{b}");
}
