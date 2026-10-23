//! Rewards by Madar: loyalty sold on its own (Madar REWARDS_DESIGN.md).
//!
//! - The add-on: plan Free + `loyalty` lifts the ceiling for loyalty only;
//!   only a super admin switches it; upgrading to paid ends it; a downgrade
//!   does not bring it back.
//! - The counter: stamps with no order, a full card completed, once per key,
//!   and every refusal (stamps only, card not full, bad counts, key reuse, no
//!   add-on).
//! - PIN sign-in with no registered device, as the teller page does it.
//!
//! Each guard is exercised from both sides, so no test passes by never
//! reaching it.

mod common;

use actix_web::{App, http::StatusCode, test, web};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use common::combos::{Shop, secret, shop, token};
use madar_rust::authz::{Cap, require::effective};
use madar_rust::models::UserRole;

macro_rules! app {
    ($pool:expr) => {{
        unsafe {
            std::env::set_var("MADAR_DISABLE_RATE_LIMIT", "1");
            std::env::set_var("MADAR_DISABLE_AUTO_TRANSLATION", "1");
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

async fn call<S, B>(app: &S, req: test::TestRequest, bearer: Option<&str>) -> (StatusCode, Value)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let req = match bearer {
        Some(b) => req.insert_header(("Authorization", format!("Bearer {b}"))),
        None => req,
    };
    let resp = test::call_service(app, req.to_request()).await;
    let status = resp.status();
    let bytes = test::read_body(resp).await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn super_admin() -> String {
    madar_rust::auth::jwt::create_token(
        &secret(),
        Uuid::new_v4(),
        None,
        UserRole::SuperAdmin,
        None,
        24,
    )
    .unwrap()
}

async fn setup(pool: &PgPool) -> Shop {
    madar_rust::permissions::seeder::seed_role_permissions(pool)
        .await
        .unwrap();
    madar_rust::authz::sync_catalogue(pool).await.unwrap();
    shop(pool).await
}

async fn set_plan(pool: &PgPool, org: Uuid, plan: &str) {
    sqlx::query("UPDATE organizations SET plan = $2 WHERE id = $1")
        .bind(org)
        .bind(plan)
        .execute(pool)
        .await
        .unwrap();
}

async fn addon_row(pool: &PgPool, org: Uuid) -> Option<(String, bool)> {
    sqlx::query_as(
        "SELECT status, ends_at IS NOT NULL FROM org_addons WHERE org_id = $1 AND feature = 'loyalty'",
    )
    .bind(org)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// A stamp card: `size` stamps fill it; `per_item` counts items, not visits.
async fn stamp_card(pool: &PgPool, org: Uuid, mode: &str, size: i32, per_item: bool) {
    sqlx::query(
        "INSERT INTO loyalty_settings \
            (org_id, branch_id, enabled, mode, earn_piastres_per_point, default_reward_cost, \
             require_otp, stamp_per_line_item, reward_any_item) \
         VALUES ($1, NULL, true, $2, 1000, $3, false, $4, true)",
    )
    .bind(org)
    .bind(mode)
    .bind(size)
    .bind(per_item)
    .execute(pool)
    .await
    .unwrap();
}

async fn member(pool: &PgPool, org: Uuid, phone: &str) -> Uuid {
    let tok = format!("M{}", &Uuid::new_v4().simple().to_string()[..22]);
    common::members::seed_loyalty_member(pool, org, phone, "Mona", &tok).await
}

async fn visits(pool: &PgPool, m: Uuid) -> i32 {
    sqlx::query_scalar("SELECT visits_balance FROM loyalty_memberships WHERE id = $1")
        .bind(m)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A loyalty-only shop: plan Free + the add-on, granted the way the super
/// admin does it.
async fn rewards_shop<S, B>(app: &S, pool: &PgPool, s: &Shop)
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    set_plan(pool, s.org, "free").await;
    let (st, body) = call(
        app,
        test::TestRequest::put()
            .uri(&format!("/orgs/{}/addons/loyalty", s.org))
            .set_json(json!({"note": "Rewards test"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

fn teller(s: &Shop) -> String {
    token(s.teller, s.org, UserRole::Teller)
}

// ── The add-on ──────────────────────────────────────────────────────────────

#[sqlx::test]
async fn the_loyalty_addon_lifts_the_free_ceiling_for_loyalty_and_nothing_else(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    set_plan(&pool, s.org, "free").await;

    let before = effective(&pool, s.teller, None).await.unwrap();
    assert!(!before.can(Cap::LoyaltyUse), "Free alone has no loyalty");

    rewards_shop(&app, &pool, &s).await;
    let after = effective(&pool, s.teller, None).await.unwrap();
    assert!(
        after.can(Cap::LoyaltyUse),
        "the add-on grants the teller loyalty.use"
    );
    assert!(after.can(Cap::LoyaltyRead));
    assert!(!after.can(Cap::InventoryRead), "and nothing else paid");
    assert!(after.can(Cap::PosSignIn), "core stays");

    // The wire: the plan stays `free`; the add-on rides beside it.
    let (_, me) = call(
        &app,
        test::TestRequest::get().uri("/auth/me"),
        Some(&teller(&s)),
    )
    .await;
    assert_eq!(me["plan"], "free");
    assert_eq!(me["addons"], json!(["loyalty"]));
    let (st, plan) = call(
        &app,
        test::TestRequest::get().uri(&format!("/orgs/{}/plan", s.org)),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{plan}");
    assert_eq!(
        plan["entitlements"]["features"],
        json!(["floor", "loyalty"])
    );
    assert_eq!(
        plan["entitlements"]["limits"]["branches"], 1,
        "limits stay Free's"
    );
    assert_eq!(plan["addons"], json!(["loyalty"]));
}

#[sqlx::test]
async fn only_a_super_admin_grants_an_addon_and_only_on_free(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    let uri = format!("/orgs/{}/addons/loyalty", s.org);

    // Paid already includes it: refused, no row.
    let (st, body) = call(
        &app,
        test::TestRequest::put().uri(&uri),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "ADDON_ON_PAID");
    assert!(addon_row(&pool, s.org).await.is_none());

    set_plan(&pool, s.org, "free").await;
    // The shop's own owner cannot sell itself an add-on.
    let (st, _) = call(
        &app,
        test::TestRequest::put().uri(&uri),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Only loyalty is sold as an add-on.
    let (st, _) = call(
        &app,
        test::TestRequest::put().uri(&format!("/orgs/{}/addons/inventory", s.org)),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(addon_row(&pool, s.org).await.is_none());

    let (st, _) = call(
        &app,
        test::TestRequest::put().uri(&uri),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        addon_row(&pool, s.org).await,
        Some(("active".into(), false))
    );

    // Ending it keeps the row and switches loyalty off.
    let (st, _) = call(
        &app,
        test::TestRequest::delete().uri(&uri),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert!(
        !effective(&pool, s.teller, None)
            .await
            .unwrap()
            .can(Cap::LoyaltyUse)
    );
}

#[sqlx::test]
async fn upgrading_ends_the_addon_and_a_downgrade_does_not_bring_it_back(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;

    // Upgrade through the super admin's PATCH, the route the dashboard uses.
    let (st, body) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "paid"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert!(
        effective(&pool, s.teller, None)
            .await
            .unwrap()
            .can(Cap::LoyaltyUse),
        "paid has it"
    );

    set_plan(&pool, s.org, "free").await;
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert!(
        !effective(&pool, s.teller, None)
            .await
            .unwrap()
            .can(Cap::LoyaltyUse),
        "back on Free the add-on must be granted again"
    );
}

#[sqlx::test]
async fn a_free_shops_card_page_exists_only_with_the_addon(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    let tok: String =
        sqlx::query_scalar("SELECT member_token FROM loyalty_memberships WHERE id = $1")
            .bind(m)
            .fetch_one(&pool)
            .await
            .unwrap();
    let uri = format!("/public/loyalty/card/{tok}");

    set_plan(&pool, s.org, "free").await;
    let (st, _) = call(&app, test::TestRequest::get().uri(&uri), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "Free alone has no card page");

    rewards_shop(&app, &pool, &s).await;
    let (st, card) = call(&app, test::TestRequest::get().uri(&uri), None).await;
    assert_eq!(st, StatusCode::OK, "{card}");
}

// ── The counter ─────────────────────────────────────────────────────────────

fn stamp_req(s: &Shop, m: Uuid, items: i32, key: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/loyalty/counter/stamp")
        .set_json(json!({
            "branch_id": s.branch, "customer_id": m, "items": items, "idempotency_key": key,
        }))
}

fn redeem_req(s: &Shop, m: Uuid, key: &str) -> test::TestRequest {
    test::TestRequest::post()
        .uri("/loyalty/counter/redeem")
        .set_json(json!({
            "branch_id": s.branch, "customer_id": m, "idempotency_key": key,
        }))
}

#[sqlx::test]
async fn a_counter_stamps_completes_and_carries_over(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    // A card of 5, a stamp per item.
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    let t = teller(&s);

    // 3 items -> 3 stamps (0 + 3 = 3 of 5).
    let (st, r) = call(&app, stamp_req(&s, m, 3, "key-aaaa-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], 3);
    assert_eq!(r["already"], false);
    assert_eq!(r["member"]["balance"], 3);
    assert_eq!(r["member"]["next_reward_cost"], 5);
    assert_eq!(visits(&pool, m).await, 3);

    // The same key again: nothing new, the first answer.
    let (st, r) = call(&app, stamp_req(&s, m, 3, "key-aaaa-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(
        (r["stamps"].as_i64(), r["already"].as_bool()),
        (Some(3), Some(true))
    );
    assert_eq!(visits(&pool, m).await, 3);

    // Not full (3 < 5): refused, nothing moves.
    let (st, r) = call(&app, redeem_req(&s, m, "key-bbbb-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "LOYALTY_CARD_NOT_FULL");
    assert_eq!(visits(&pool, m).await, 3);

    // 4 more -> 7; complete -> 7 - 5 = 2 carried over.
    call(&app, stamp_req(&s, m, 4, "key-aaaa-0002"), Some(&t)).await;
    assert_eq!(visits(&pool, m).await, 7);
    let (st, r) = call(&app, redeem_req(&s, m, "key-bbbb-0002"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], -5);
    assert_eq!(r["member"]["balance"], 2);
    // A retried completion does not take a second card.
    let (_, r) = call(&app, redeem_req(&s, m, "key-bbbb-0002"), Some(&t)).await;
    assert_eq!(
        (r["stamps"].as_i64(), r["already"].as_bool()),
        (Some(-5), Some(true))
    );
    assert_eq!(visits(&pool, m).await, 2);

    // The ledger: counter rows, no order, the teller's branch, one per key.
    let rows: Vec<(String, String, i32, Option<Uuid>, Uuid)> = sqlx::query_as(
        "SELECT kind::text, source, points, order_id, branch_id FROM loyalty_transactions
          WHERE customer_id = $1 ORDER BY created_at, points DESC",
    )
    .bind(m)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("earn".into(), "counter".into(), 3, None, s.branch),
            ("earn".into(), "counter".into(), 4, None, s.branch),
            ("redeem".into(), "counter".into(), -5, None, s.branch),
        ]
    );

    // History reads them (old tills decode `source` as a string, branch as a uuid).
    let (st, h) = call(
        &app,
        test::TestRequest::get().uri(&format!("/loyalty/members/{m}")),
        Some(&s.admin_token()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{h}");
}

#[sqlx::test]
async fn a_visit_card_counts_one_stamp_per_visit_when_items_are_not_counted(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    stamp_card(&pool, s.org, "visits", 5, false).await;
    let m = member(&pool, s.org, "01001234567").await;
    let (st, r) = call(
        &app,
        stamp_req(&s, m, 3, "key-cccc-0001"),
        Some(&teller(&s)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["stamps"], 1, "per-visit card: 3 items are one visit");
}

#[sqlx::test]
async fn the_counter_refuses_what_it_must(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    let m = member(&pool, s.org, "01001234567").await;
    let other = member(&pool, s.org, "01001234568").await;
    let t = teller(&s);

    // Free without the add-on: no loyalty at all.
    set_plan(&pool, s.org, "free").await;
    stamp_card(&pool, s.org, "points", 100, false).await;
    let (st, r) = call(&app, stamp_req(&s, m, 1, "key-dddd-0001"), Some(&t)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{r}");

    rewards_shop(&app, &pool, &s).await;
    // A points programme: the counter stamps cards only.
    let (st, r) = call(&app, stamp_req(&s, m, 1, "key-dddd-0002"), Some(&t)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "LOYALTY_COUNTER_STAMPS_ONLY");

    sqlx::query(
        "UPDATE loyalty_settings SET mode = 'visits', default_reward_cost = 5 WHERE org_id = $1",
    )
    .bind(s.org)
    .execute(&pool)
    .await
    .unwrap();
    // Counts outside 1..=50, and a malformed key.
    for (items, key) in [
        (0, "key-dddd-0003"),
        (51, "key-dddd-0004"),
        (-2, "key-dddd-0005"),
        (1, "short"),
    ] {
        let (st, r) = call(&app, stamp_req(&s, m, items, key), Some(&t)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{items} {key}: {r}");
    }
    let (st, _) = call(&app, stamp_req(&s, m, 50, "key-dddd-0006"), Some(&t)).await;
    assert_eq!(st, StatusCode::OK, "50 is the documented bound");

    // One key, another member: a client bug, refused rather than answered.
    let (st, r) = call(&app, stamp_req(&s, other, 50, "key-dddd-0006"), Some(&t)).await;
    assert_eq!(st, StatusCode::CONFLICT, "{r}");
    assert_eq!(r["code"], "LOYALTY_KEY_REUSED");
    assert_eq!(visits(&pool, other).await, 0);

    // A member of another shop is not found here.
    let s2 = shop(&pool).await;
    let stranger = member(&pool, s2.org, "01001234569").await;
    let (st, _) = call(&app, stamp_req(&s, stranger, 1, "key-dddd-0007"), Some(&t)).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ── PIN sign-in with no device ──────────────────────────────────────────────

#[sqlx::test]
async fn a_teller_signs_in_with_a_pin_and_no_registered_device(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    sqlx::query("UPDATE users SET pin_hash = $2 WHERE id = $1")
        .bind(s.teller)
        .bind(bcrypt::hash("4821", 4).unwrap())
        .execute(&pool)
        .await
        .unwrap();

    // The teller page sends a per-browser id that no device row knows.
    let (st, r) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .insert_header(("X-Madar-Device-Id", Uuid::new_v4().to_string()))
            .set_json(json!({"pin": "4821", "branch_id": s.branch})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["plan"], "free");
    assert_eq!(r["addons"], json!(["loyalty"]));
    assert_eq!(r["user"]["branch_id"], json!(s.branch));

    // A wrong PIN is still refused.
    let (st, _) = call(
        &app,
        test::TestRequest::post()
            .uri("/auth/login")
            .set_json(json!({"pin": "1111", "branch_id": s.branch})),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

// ── Upgrade: same ledger, and orders earn into it ───────────────────────────

#[sqlx::test]
async fn after_upgrading_the_counters_stamps_stay_and_orders_earn_on_top(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    rewards_shop(&app, &pool, &s).await;
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    let t = teller(&s);
    call(&app, stamp_req(&s, m, 2, "key-eeee-0001"), Some(&t)).await;
    assert_eq!(visits(&pool, m).await, 2);

    // The super admin moves the shop to paid: the add-on ends, nothing moves.
    let (st, _) = call(
        &app,
        test::TestRequest::patch()
            .uri(&format!("/orgs/{}", s.org))
            .set_json(json!({"plan": "paid"})),
        Some(&super_admin()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(addon_row(&pool, s.org).await, Some(("ended".into(), true)));
    assert_eq!(visits(&pool, m).await, 2, "balance intact");

    // A till's sale now earns into the same card. An order with no lines in a
    // per-item programme earns one stamp (earn::points_for_order's floor):
    // 2 + 1 = 3.
    let order = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, branch_id, teller_id, till_id, idempotency_key, customer_name, subtotal, \
             discount_amount, tax_amount, total_amount, status, order_number, payment_method, order_ref) \
         VALUES ($1, $2, $3, $4, gen_random_uuid(), 'Customer', 500, 0, 0, 500, 'completed', 1, 'cash', gen_random_uuid()::text)",
    )
    .bind(order)
    .bind(s.branch)
    .bind(s.teller)
    .bind(s.teller_till)
    .execute(&pool)
    .await
    .unwrap();
    let (st, r) = call(
        &app,
        test::TestRequest::post().uri("/loyalty/award").set_json(json!({
            "branch_id": s.branch, "order_id": order, "customer_id": m,
        })),
        Some(&t),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["points_awarded"], 1);
    assert_eq!(visits(&pool, m).await, 3);
    let sources: Vec<String> = sqlx::query_scalar(
        "SELECT source FROM loyalty_transactions WHERE customer_id = $1 ORDER BY created_at",
    )
    .bind(m)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(sources, vec!["counter", "sale"]);
}

// ── No permission is a 403 before any input is judged ───────────────────────

#[sqlx::test]
async fn a_teller_without_loyalty_gets_403_before_any_400(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    let m = member(&pool, s.org, "01001234567").await;
    let t = teller(&s);
    // Free without the add-on: the teller does not hold `loyalty.use`.
    set_plan(&pool, s.org, "free").await;
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let bad = [
        stamp_req(&s, m, 1, "short"),
        stamp_req(&s, m, 0, "key-ffff-0001"),
        stamp_req(&s, m, 51, "key-ffff-0002"),
        redeem_req(&s, m, "short"),
    ];
    for req in bad {
        let (st, r) = call(&app, req, Some(&t)).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "no permission is 403, never 400: {r}");
    }

    // With it, the same requests are the 400s they always were.
    rewards_shop(&app, &pool, &s).await;
    let bad = [
        stamp_req(&s, m, 1, "short"),
        stamp_req(&s, m, 0, "key-ffff-0001"),
        stamp_req(&s, m, 51, "key-ffff-0002"),
        redeem_req(&s, m, "short"),
    ];
    for req in bad {
        let (st, r) = call(&app, req, Some(&t)).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{r}");
    }
    assert_eq!(visits(&pool, m).await, 0, "nothing was stamped");
}

// ── Apple's web service: Last-Modified is never missing ─────────────────────

/// Configure Apple signing with a throwaway self-signed certificate (iOS would
/// refuse it, which is right; the server path is real). nextest runs each test
/// in its own process, so the environment is this test's alone.
fn throwaway_apple_signing() {
    let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
    let key = openssl::pkey::PKey::from_rsa(rsa).unwrap();
    let mut name = openssl::x509::X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "madar-test").unwrap();
    let name = name.build();
    let mut b = openssl::x509::X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_subject_name(&name).unwrap();
    b.set_issuer_name(&name).unwrap();
    b.set_pubkey(&key).unwrap();
    b.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap()).unwrap();
    b.set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap()).unwrap();
    b.sign(&key, openssl::hash::MessageDigest::sha256()).unwrap();
    let cert = String::from_utf8(b.build().to_pem().unwrap()).unwrap();
    // SAFETY: one test per process (nextest); nothing else reads these here.
    unsafe {
        std::env::set_var("LOYALTY_APPLE_PASS_TYPE_ID", "pass.cloud.madar-pos.loyalty");
        std::env::set_var("LOYALTY_APPLE_TEAM_ID", "TEAM123456");
        std::env::set_var("LOYALTY_APPLE_CERT_PEM", &cert);
        std::env::set_var(
            "LOYALTY_APPLE_KEY_PEM",
            String::from_utf8(key.private_key_to_pem_pkcs8().unwrap()).unwrap(),
        );
        std::env::set_var("LOYALTY_APPLE_WWDR_PEM", &cert);
    }
}

#[sqlx::test]
async fn the_pass_always_carries_last_modified_and_a_stale_card_is_never_a_304(pool: PgPool) {
    throwaway_apple_signing();
    let s = setup(&pool).await;
    let app = app!(pool);
    stamp_card(&pool, s.org, "visits", 5, true).await;
    let m = member(&pool, s.org, "01001234567").await;
    sqlx::query(
        "UPDATE loyalty_memberships SET apple_auth_token = 'tok-rewards-1', pass_updated_at = NULL WHERE id = $1",
    )
    .bind(m)
    .execute(&pool)
    .await
    .unwrap();
    let uri = format!("/wallet/v1/passes/pass.cloud.madar-pos.loyalty/{m}");
    let get = |since: Option<&str>| {
        let r = test::TestRequest::get()
            .uri(&uri)
            .insert_header(("Authorization", "ApplePass tok-rewards-1"));
        match since {
            Some(h) => r.insert_header(("If-Modified-Since", h.to_string())),
            None => r,
        }
    };

    // Stale (NULL): a fresh pass, WITH Last-Modified, whatever the device says.
    let resp = test::call_service(&app, get(None).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let lm = resp.headers().get("Last-Modified").expect("Last-Modified on a stale card");
    chrono::DateTime::parse_from_rfc2822(lm.to_str().unwrap()).expect("RFC 2822");
    let future = (chrono::Utc::now() + chrono::Duration::days(365)).to_rfc2822();
    let resp = test::call_service(&app, get(Some(&future)).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK, "a NULL stamp is never a 304");
    assert!(resp.headers().get("Last-Modified").is_some());

    // Stamped: exactly that time, and a 304 when the device already has it.
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-01T09:30:00Z").unwrap().with_timezone(&chrono::Utc);
    sqlx::query("UPDATE loyalty_memberships SET pass_updated_at = $2 WHERE id = $1")
        .bind(m)
        .bind(at)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(&app, get(None).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("Last-Modified").unwrap().to_str().unwrap(), at.to_rfc2822());
    let resp = test::call_service(&app, get(Some(&at.to_rfc2822())).to_request()).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    let earlier = (at - chrono::Duration::hours(1)).to_rfc2822();
    let resp = test::call_service(&app, get(Some(&earlier)).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // A stamp with microseconds (what now() stores): the header the device
    // echoes back is whole seconds, and it still gets its 304.
    sqlx::query("UPDATE loyalty_memberships SET pass_updated_at = '2026-10-21 14:13:05.987654+00' WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let resp = test::call_service(&app, get(None).to_request()).await;
    let lm = resp.headers().get("Last-Modified").unwrap().to_str().unwrap().to_string();
    let resp = test::call_service(&app, get(Some(&lm)).to_request()).await;
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "the echoed Last-Modified is a 304");
    // And a second earlier is still a fresh pass.
    let before = (chrono::DateTime::parse_from_rfc2822(&lm).unwrap() - chrono::Duration::seconds(1)).to_rfc2822();
    let resp = test::call_service(&app, get(Some(&before)).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[sqlx::test]
async fn a_stale_card_is_listed_to_its_device_whatever_it_last_saw(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    let m = member(&pool, s.org, "01001234567").await;
    sqlx::query(
        "INSERT INTO loyalty_pass_devices (device_library_id, customer_id, org_id, push_token) \
         VALUES ('dev-1', $1, $2, 'push-1')",
    )
    .bind(m)
    .bind(s.org)
    .execute(&pool)
    .await
    .unwrap();
    let since = urlencode(&chrono::Utc::now().to_rfc3339());
    let list = |since: &str| {
        test::TestRequest::get().uri(&format!(
            "/wallet/v1/devices/dev-1/registrations/pass.cloud.madar-pos.loyalty?passesUpdatedSince={since}"
        ))
    };

    // NULL (marked stale): listed even though the device asked "since now".
    sqlx::query("UPDATE loyalty_memberships SET pass_updated_at = NULL WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let (st, body) = call(&app, list(&since), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["serialNumbers"], json!([m.to_string()]));

    // Stamped before "since": nothing changed, 204 as before.
    sqlx::query("UPDATE loyalty_memberships SET pass_updated_at = now() - interval '1 day' WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let (st, _) = call(&app, list(&since), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // The tag a device is handed goes back, unencoded, in the query string,
    // and must still mean "since then": a changed card is listed with a NEWER
    // tag, and asking again with that tag is a 204.
    sqlx::query("UPDATE loyalty_memberships SET pass_updated_at = now() WHERE id = $1")
        .bind(m)
        .execute(&pool)
        .await
        .unwrap();
    let (st, body) = call(&app, list(&since), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let tag = body["lastUpdated"].as_str().unwrap().to_string();
    assert!(!tag.contains('+'), "a tag that survives a query string: {tag}");
    let (st, body) = call(&app, list(&tag), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "the same tag back means nothing changed: {body}");
    // A tag issued before this fix ("+00:00", unencoded: the + arrives as a
    // space) is still read as a time, not as no time at all.
    let old_tag = chrono::DateTime::parse_from_rfc3339(&tag).unwrap().to_rfc3339();
    assert!(old_tag.contains("+00:00"));
    let (st, body) = call(&app, list(&old_tag), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "an old-style tag is understood: {body}");
}

fn urlencode(s: &str) -> String {
    s.replace('+', "%2B").replace(':', "%3A")
}

// ── The join page: social proof and the stamp unit ──────────────────────────

async fn join_info(app: &impl actix_web::dev::Service<actix_http::Request, Response = actix_web::dev::ServiceResponse<impl actix_web::body::MessageBody>, Error = actix_web::Error>, s: &Shop) -> Value {
    let (st, body) = call(
        app,
        test::TestRequest::get().uri(&format!("/public/loyalty/join-info?org_id={}", s.org)),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

#[sqlx::test]
async fn the_join_page_shows_a_rounded_member_count_only_past_a_hundred(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamp_card(&pool, s.org, "visits", 6, false).await;
    // Exactly 100 live members: nothing shown (absent, not null).
    let mut members = Vec::new();
    for i in 0..100 {
        members.push(member(&pool, s.org, &format!("010{:08}", 10_000_000 + i)).await);
    }
    let info = join_info(&app, &s).await;
    assert!(info.get("member_count").is_none(), "{info}");

    // The 101st: still cached for this org (the count is not read per view).
    member(&pool, s.org, "01099999999").await;
    assert!(join_info(&app, &s).await.get("member_count").is_none(), "served from the cache");
    madar_rust::loyalty::social::forget(s.org).await;
    assert_eq!(join_info(&app, &s).await["member_count"], 100, "101 shows as 100+");

    // A leaver (deleted_at, as leaving and erasure both set) is not counted.
    sqlx::query("UPDATE loyalty_memberships SET deleted_at = now() WHERE id = $1")
        .bind(members[0])
        .execute(&pool)
        .await
        .unwrap();
    madar_rust::loyalty::social::forget(s.org).await;
    assert!(join_info(&app, &s).await.get("member_count").is_none(), "back to 100");
}

#[sqlx::test]
async fn the_join_page_says_what_a_stamp_is_for(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    // Points: no stamp unit at all.
    stamp_card(&pool, s.org, "points", 100, false).await;
    assert!(join_info(&app, &s).await.get("stamp_unit").is_none());
    let set = |mode: &'static str, per_item: bool| {
        let pool = pool.clone();
        let org = s.org;
        async move {
            sqlx::query("UPDATE loyalty_settings SET mode = $2, stamp_per_line_item = $3 WHERE org_id = $1")
                .bind(org)
                .bind(mode)
                .bind(per_item)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    set("visits", false).await;
    assert_eq!(join_info(&app, &s).await["stamp_unit"], "order");
    set("visits", true).await;
    assert_eq!(join_info(&app, &s).await["stamp_unit"], "item");
    // A Rewards by Madar counter shop (Free + the loyalty add-on).
    rewards_shop(&app, &pool, &s).await;
    assert_eq!(join_info(&app, &s).await["stamp_unit"], "stamp");
}

#[sqlx::test]
async fn the_card_page_and_the_join_answer_say_what_a_stamp_is_for(pool: PgPool) {
    let s = setup(&pool).await;
    let app = app!(pool);
    stamp_card(&pool, s.org, "points", 100, false).await;
    sqlx::query("UPDATE loyalty_settings SET require_otp = false WHERE org_id = $1")
        .bind(s.org)
        .execute(&pool)
        .await
        .unwrap();
    let m = member(&pool, s.org, "01001234567").await;
    let tok: String = sqlx::query_scalar("SELECT member_token FROM loyalty_memberships WHERE id = $1")
        .bind(m)
        .fetch_one(&pool)
        .await
        .unwrap();
    let card = |app_tok: String| test::TestRequest::get().uri(&format!("/public/loyalty/card/{app_tok}"));
    let join = |phone: &str| {
        test::TestRequest::post().uri("/public/loyalty/join").set_json(json!({
            "org_id": s.org, "name": "Hoda", "phone": phone,
        }))
    };
    let set = |mode: &'static str, per_item: bool| {
        let pool = pool.clone();
        let org = s.org;
        async move {
            sqlx::query("UPDATE loyalty_settings SET mode = $2, stamp_per_line_item = $3 WHERE org_id = $1")
                .bind(org)
                .bind(mode)
                .bind(per_item)
                .execute(&pool)
                .await
                .unwrap();
        }
    };

    // Points: absent on both.
    let (st, body) = call(&app, card(tok.clone()), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.get("stamp_unit").is_none(), "{body}");
    let (st, body) = call(&app, join("01001234570"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(body.get("stamp_unit").is_none(), "{body}");

    for (mode, per_item, phone, want) in [
        ("visits", false, "01001234571", "order"),
        ("visits", true, "01001234572", "item"),
    ] {
        set(mode, per_item).await;
        let (_, body) = call(&app, card(tok.clone()), None).await;
        assert_eq!(body["stamp_unit"], want, "card: {body}");
        let (_, body) = call(&app, join(phone), None).await;
        assert_eq!(body["stamp_unit"], want, "join: {body}");
    }

    // A counter shop (Free + the add-on).
    rewards_shop(&app, &pool, &s).await;
    let (_, body) = call(&app, card(tok), None).await;
    assert_eq!(body["stamp_unit"], "stamp", "card: {body}");
    let (_, body) = call(&app, join("01001234573"), None).await;
    assert_eq!(body["stamp_unit"], "stamp", "join: {body}");
}
